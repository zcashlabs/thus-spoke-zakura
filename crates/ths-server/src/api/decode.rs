use super::error::{ApiError, ApiJson, ApiResult};
use axum::Json;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Cursor;
use zcash_primitives::transaction::{Transaction, TxVersion};
use zcash_protocol::consensus::BranchId;

// a transaction cannot exceed the 2 MB block size, so its hex and the json
// around it fit in 4 MiB
pub(super) const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
pub(super) struct DecodeRequest {
    hex: String,
}

pub(super) async fn decode(ApiJson(request): ApiJson<DecodeRequest>) -> ApiResult<Json<Value>> {
    let bytes = hex::decode(&request.hex)
        .map_err(|error| ApiError::bad_request(format!("invalid hex: {error}")))?;
    let mut cursor = Cursor::new(bytes.as_slice());
    // v1 to v4 transactions do not encode a branch id, so nu6.3 only lets the
    // parser read them; the response reports a branch for v5 and later
    let tx = Transaction::read(&mut cursor, BranchId::Nu6_3)
        .map_err(|error| ApiError::bad_request(format!("invalid transaction: {error}")))?;
    if cursor.position() != bytes.len() as u64 {
        return Err(ApiError::bad_request(
            "transaction hex contains trailing bytes",
        ));
    }

    let (inputs, outputs): (Vec<_>, Vec<_>) = tx.transparent_bundle().map_or_else(
        || (Vec::new(), Vec::new()),
        |bundle| {
            (
                bundle
                    .vin
                    .iter()
                    .map(|input| {
                        json!({
                            "txid": input.prevout().txid().to_string(),
                            "vout": input.prevout().n(),
                        })
                    })
                    .collect(),
                bundle
                    .vout
                    .iter()
                    .map(|output| {
                        json!({
                            "value_zatoshi": u64::from(output.value()),
                            "script_pubkey": hex::encode(&output.script_pubkey().0.0),
                        })
                    })
                    .collect(),
            )
        },
    );

    let sprout = tx.sprout_bundle();
    let sapling = tx.sapling_bundle();
    let orchard = tx.orchard_bundle();
    let ironwood = tx.ironwood_bundle();
    let version = tx.version();
    let branch_id = matches!(version, TxVersion::V5 | TxVersion::V6)
        .then(|| format!("0x{:08x}", u32::from(tx.consensus_branch_id())));

    Ok(Json(json!({
        "txid": tx.txid().to_string(),
        "version": version.header() & 0x7fff_ffff,
        "branch_id": branch_id,
        "expiry_height": u32::from(tx.expiry_height()),
        "transparent_inputs": inputs,
        "transparent_outputs": outputs,
        "shielded_action_counts": {
            "sprout_joinsplits": sprout.map_or(0, |bundle| bundle.joinsplits.len()),
            "sapling_spends": sapling.map_or(0, |bundle| bundle.shielded_spends().len()),
            "sapling_outputs": sapling.map_or(0, |bundle| bundle.shielded_outputs().len()),
            "orchard_actions": orchard.map_or(0, |bundle| bundle.actions().len()),
            "ironwood_actions": ironwood.map_or(0, |bundle| bundle.actions().len()),
        },
        "public_value_balances": {
            "sprout_zatoshi": sprout.map_or(Some(0), |bundle| bundle.value_balance().map(i64::from)),
            "sapling_zatoshi": sapling.map_or(0, |bundle| i64::from(*bundle.value_balance())),
            "orchard_zatoshi": orchard.map_or(0, |bundle| i64::from(*bundle.value_balance())),
            "ironwood_zatoshi": ironwood.map_or(0, |bundle| i64::from(*bundle.value_balance())),
        },
    })))
}

#[cfg(test)]
#[path = "../../tests/unit/api/decode/tests.rs"]
mod tests;
