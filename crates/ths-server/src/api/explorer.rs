use super::error::{ApiError, ApiPath, ApiQuery, ApiResult, not_found};
use crate::db::USER_ACCOUNT_COUNT;
use crate::network::regtest_network;
use crate::state::AppState;
use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use zcash_keys::address::Address;
use zcash_protocol::TxId;
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
) -> bool {
    let Some(vin) = tx.get_mut("vin").and_then(Value::as_array_mut) else {
        return true;
    };
    let mut complete = true;
    for input in vin {
        if input.get("coinbase").is_some() {
            continue;
        }
        let out = input
            .get("txid")
            .and_then(Value::as_str)
            .and_then(|txid| prev_txs.get(txid))
            .and_then(|prev| prev.get("vout"))
            .and_then(Value::as_array)
            .and_then(|vout| {
                let n = input.get("vout").and_then(Value::as_u64)?;
                vout.iter()
                    .find(|out| out.get("n").and_then(Value::as_u64) == Some(n))
                    .or_else(|| vout.get(n as usize))
            });
        if let Some(out) = out
            && let (Some(value_zat), Some(script)) = (
                out.get("valueZat").cloned(),
                out.get("scriptPubKey").cloned(),
            )
        {
            input["valueZat"] = value_zat;
            input["scriptPubKey"] = script;
            continue;
        }
        input["prevout_unavailable"] = Value::Bool(true);
        complete = false;
    }
    complete
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
        match state.0.rpc.transaction(&prev_id).await {
            Ok(prev) => {
                prev_txs.insert(prev_id, prev);
            }
            Err(error) => tracing::warn!(
                error = %format_args!("{error:#}"),
                transaction = %txid,
                previous_transaction = %prev_id,
                "resolving a transparent input failed"
            ),
        }
    }
    let complete = attach_transparent_prevouts(&mut tx, &prev_txs);
    if let Some(tx) = tx.as_object_mut() {
        tx.insert(
            "transparent_inputs_complete".to_owned(),
            Value::Bool(complete),
        );
    }
    Ok(Json(tx))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WalletTransactionQuery {
    account: Option<i64>,
}

/// Parses a transaction ID in the byte-reversed hex form Zakura and the
/// dashboard display.
fn parse_txid(txid: &str) -> Option<TxId> {
    let mut bytes: [u8; 32] = hex::decode(txid).ok()?.try_into().ok()?;
    bytes.reverse();
    Some(TxId::from_bytes(bytes))
}

/// What the local wallet recorded about one transaction for the development
/// accounts: outputs, change, memos, spends and fee. Public chain data comes
/// from `transaction`.
pub(crate) async fn wallet_transaction(
    State(state): State<AppState>,
    ApiPath(txid): ApiPath<String>,
    ApiQuery(query): ApiQuery<WalletTransactionQuery>,
) -> ApiResult<Json<Value>> {
    let account = query
        .account
        .map(|account| {
            u8::try_from(account)
                .ok()
                .filter(|account| (1..=USER_ACCOUNT_COUNT).contains(account))
                .ok_or_else(|| {
                    ApiError::bad_request("account must be a development account from 1 to 5")
                })
        })
        .transpose()?;
    let not_found_error = || ApiError {
        status: StatusCode::NOT_FOUND,
        message: NO_TRANSACTION.into(),
    };
    let id = parse_txid(&txid).ok_or_else(not_found_error)?;
    // Zakura answers with lowercase hex, whatever case the caller used.
    let txid = id.to_string();
    let tx = state
        .0
        .rpc
        .lookup_transaction(&txid)
        .await?
        .ok_or_else(not_found_error)?;
    // Zakura omits confirmations for a mempool transaction.
    let confirmations = tx
        .get("confirmations")
        .and_then(Value::as_i64)
        .filter(|confirmations| *confirmations > 0);
    let mined_height = confirmations.and_then(|_| tx.get("height").and_then(Value::as_u64));
    let mut view = state.0.wallet.wallet_transaction(id).await?;
    if let Some(account) = account {
        view = view.for_account(account);
    }
    Ok(Json(json!({
        "txid": txid,
        "status": if mined_height.is_some() { "confirmed" } else { "pending" },
        "mined_height": mined_height,
        "confirmations": confirmations.unwrap_or(0),
        "fee_zatoshi": view.fee_zatoshi,
        "details": view.details,
        "outputs": view.outputs,
        "spends": view.spends,
    })))
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
    // An empty value is not a height, hash, transaction id, or address.
    // Asking Zakura would report that client mistake as a node failure.
    let q = query.q.trim();
    if q.is_empty() {
        return Err(ApiError::bad_request(
            "q must be a block height, block hash, transaction id, or transparent address",
        ));
    }
    // Trim only to choose the address path. The raw query is what gets
    // validated there, so an address with surrounding whitespace stays a
    // client error instead of reaching `getaddressbalance`.
    if q.starts_with('t') {
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

fn require_transparent_address(value: &str) -> ApiResult<()> {
    // `Address::decode` trims, so a trailing newline would be accepted and
    // then fail when the raw value is sent to Zakura.
    if value.trim() != value {
        return Err(ApiError::bad_request(
            "address must not have leading or trailing whitespace",
        ));
    }
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
