use super::error::{ApiError, ApiPath, ApiQuery, ApiResult, not_found};
use crate::network::regtest_network;
use crate::state::AppState;
use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use zcash_keys::address::Address;
pub(crate) async fn block(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<Value>> {
    let mut block = state
        .0
        .rpc
        .block(&id)
        .await
        .map_err(|e| not_found(e, NO_BLOCK))?;
    // `getblock` has no Ironwood root, but `z_gettreestate` does. The root is
    // an extra detail, so a failed lookup still returns the block.
    if let Some(hash) = block.get("hash").and_then(Value::as_str).map(str::to_owned) {
        match state.0.rpc.treestate(&hash).await {
            Ok(treestate) => add_ironwood_root(&mut block, &treestate),
            Err(error) => tracing::warn!(
                error = %format_args!("{error:#}"),
                %hash,
                "reading the Ironwood treestate failed"
            ),
        }
    }
    Ok(Json(block))
}
/// Copies the Ironwood note commitment root from a `z_gettreestate` response
/// into the block as `finalironwoodroot`, next to Zakura's `finalorchardroot`.
/// The genesis block (before NU6.3 activates at height 1) has no Ironwood root and
/// stays unchanged.
fn add_ironwood_root(block: &mut Value, treestate: &Value) {
    let root = treestate
        .pointer("/ironwood/commitments/finalRoot")
        .and_then(Value::as_str);
    if let (Some(root), Some(block)) = (root, block.as_object_mut()) {
        block.insert("finalironwoodroot".to_owned(), Value::from(root));
    }
}
#[derive(Deserialize)]
pub(crate) struct BlocksQuery {
    limit: Option<u32>,
    before: Option<u64>,
}
pub(crate) async fn blocks(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<BlocksQuery>,
) -> ApiResult<Json<Value>> {
    let info = state.0.rpc.chain_info().await?;
    let end = query.before.unwrap_or(info.blocks).min(info.blocks);
    let limit = query.limit.unwrap_or(20).clamp(1, 50) as u64;
    let start = end.saturating_sub(limit.saturating_sub(1));
    let mut page = Vec::new();
    for height in (start..=end).rev() {
        page.push(state.0.rpc.block(&height.to_string()).await?);
    }
    Ok(Json(
        json!({"blocks":page,"next_before":start.checked_sub(1)}),
    ))
}
fn transparent_prevout_ids(tx: &Value) -> Vec<String> {
    let Some(vin) = tx.get("vin").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for input in vin {
        if input.get("coinbase").is_some() {
            continue;
        }
        let Some(txid) = input.get("txid").and_then(Value::as_str) else {
            continue;
        };
        if !ids.iter().any(|id| id == txid) {
            ids.push(txid.to_owned());
        }
    }
    ids
}

/// Node `getrawtransaction` does not include the spent output. Copy address
/// and value from the previous transaction so the explorer can list inputs.
fn attach_transparent_prevouts(
    tx: &mut Value,
    prev_txs: &std::collections::HashMap<String, Value>,
) {
    let Some(vin) = tx.get_mut("vin").and_then(Value::as_array_mut) else {
        return;
    };
    for input in vin {
        if input.get("coinbase").is_some() {
            continue;
        }
        let Some(prev_txid) = input.get("txid").and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        let Some(n) = input.get("vout").and_then(Value::as_u64) else {
            continue;
        };
        let Some(prev) = prev_txs.get(&prev_txid) else {
            continue;
        };
        let Some(vout) = prev.get("vout").and_then(Value::as_array) else {
            continue;
        };
        let Some(out) = vout
            .iter()
            .find(|out| out.get("n").and_then(Value::as_u64) == Some(n))
            .or_else(|| vout.get(n as usize))
        else {
            continue;
        };
        if let Some(value_zat) = out.get("valueZat").cloned() {
            input["valueZat"] = value_zat;
        }
        if let Some(script) = out.get("scriptPubKey").cloned() {
            input["scriptPubKey"] = script;
        }
    }
}

pub(crate) async fn transaction(
    State(state): State<AppState>,
    ApiPath(txid): ApiPath<String>,
) -> ApiResult<Json<Value>> {
    let mut tx = state
        .0
        .rpc
        .transaction(&txid)
        .await
        .map_err(|e| not_found(e, NO_TRANSACTION))?;
    let mut prev_txs = std::collections::HashMap::new();
    for prev_id in transparent_prevout_ids(&tx) {
        if let Ok(prev) = state.0.rpc.transaction(&prev_id).await {
            prev_txs.insert(prev_id, prev);
        }
    }
    attach_transparent_prevouts(&mut tx, &prev_txs);
    Ok(Json(tx))
}
pub(crate) async fn mempool(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"transactions":state.0.rpc.mempool().await?})))
}
pub(crate) async fn address(
    State(state): State<AppState>,
    ApiPath(address): ApiPath<String>,
) -> ApiResult<Json<Value>> {
    require_transparent_address(&address)?;
    let balance: Value = state
        .0
        .rpc
        .call("getaddressbalance", json!([{"addresses":[address]}]))
        .await?;
    Ok(Json(json!({"address":address,"balance":balance})))
}
#[derive(Deserialize)]
pub(crate) struct SearchQuery {
    q: String,
}
pub(crate) async fn search(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<SearchQuery>,
) -> ApiResult<Json<Value>> {
    if query.q.starts_with('t') {
        return address(State(state), ApiPath(query.q)).await;
    }
    // Only a 64-character hash can also be a txid; anything else is answered by the block lookup.
    // Other node failures must surface rather than fall through to the transaction lookup.
    match state.0.rpc.block(&query.q).await {
        Ok(block) => return Ok(Json(json!({"type":"block","value":block}))),
        Err(error) => {
            let error = not_found(error, NO_BLOCK);
            if query.q.len() != 64 || error.status != StatusCode::NOT_FOUND {
                return Err(error);
            }
        }
    }
    let tx = state
        .0
        .rpc
        .transaction(&query.q)
        .await
        .map_err(|e| not_found(e, NO_BLOCK_OR_TRANSACTION))?;
    Ok(Json(json!({"type":"transaction","value":tx})))
}

pub(crate) const NO_BLOCK: &str = "No block at that height or hash on this chain.";
pub(crate) const NO_TRANSACTION: &str =
    "No transaction with that ID on this chain. It may not have been mined yet.";
const NO_BLOCK_OR_TRANSACTION: &str = "No block or transaction on this chain has that hash.";

/// Zakura answers an unknown or unparseable block or transaction with -5 or -8.
fn require_transparent_address(value: &str) -> ApiResult<()> {
    match Address::decode(&regtest_network(), value) {
        Some(Address::Transparent(_)) => Ok(()),
        _ => Err(ApiError::bad_request(
            "only transparent addresses have public explorer activity",
        )),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/api/explorer/tests.rs"]
pub(crate) mod tests;
