use super::error::{ApiError, ApiJson, ApiQuery, ApiResult};
use super::payments::require_key;
use crate::state::AppState;
use crate::{
    db::{Account, Activity, USER_ACCOUNT_COUNT},
    rpc::ChainCheckpoint,
    rpc::ChainInfo,
    state::{WalletSyncStatus, wallet_matches_tip},
};
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub(crate) async fn health(State(state): State<AppState>) -> Response {
    let (node, lightwalletd) =
        tokio::join!(state.0.rpc.chain_info(), state.0.wallet.latest_checkpoint());
    let node = node.ok();
    let wallet = state.wallet_sync_status().await;
    let ok = state.started()
        && node.is_some()
        && lightwalletd.is_ok()
        && wallet.last_success_at.is_some();
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({"ok": ok, "instance": state.0.instance, "node": node, "wallet_sync": wallet})),
    )
        .into_response()
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
struct SettlementStatus {
    node_height: Option<u64>,
    lightwalletd_height: Option<u64>,
    wallet_scanned_height: Option<u64>,
    wallet_sync_state: &'static str,
    settled: bool,
    settlement_blocker: Option<&'static str>,
}

fn settlement_status(
    node: Option<&ChainCheckpoint>,
    lightwalletd: Option<&ChainCheckpoint>,
    wallet: &WalletSyncStatus,
    started: bool,
) -> SettlementStatus {
    let settlement_blocker = if node.is_none() {
        Some("node_unavailable")
    } else if lightwalletd.is_none() {
        Some("lightwalletd_unavailable")
    } else if wallet.state == "error" {
        Some("wallet_error")
    } else if wallet.state != "ready" {
        Some("wallet_syncing")
    } else if node.map(|tip| tip.height) != lightwalletd.map(|tip| tip.height) {
        Some("height_mismatch")
    } else if node != lightwalletd {
        Some("chain_mismatch")
    } else if !node.is_some_and(|tip| wallet_matches_tip(wallet, tip)) {
        Some("wallet_behind")
    } else if !started {
        Some("starting")
    } else {
        None
    };

    SettlementStatus {
        node_height: node.map(|tip| tip.height),
        lightwalletd_height: lightwalletd.map(|tip| tip.height),
        wallet_scanned_height: wallet.fully_scanned_height,
        wallet_sync_state: wallet.state,
        settled: settlement_blocker.is_none(),
        settlement_blocker,
    }
}

#[derive(Serialize)]
pub(crate) struct Status {
    instance: String,
    node: Option<ChainInfo>,
    account_count: usize,
    auto_mine: bool,
    network: &'static str,
    endpoints: PublicEndpoints,
    wallet_sync: WalletSyncStatus,
    #[serde(flatten)]
    settlement: SettlementStatus,
}

#[derive(Serialize)]
pub(crate) struct PublicEndpoints {
    dashboard: String,
    zakura_rpc: String,
    lightwalletd: String,
    p2p: String,
}

fn public_dashboard_endpoint(configured: Option<&str>) -> String {
    configured.unwrap_or("http://127.0.0.1:8080").to_owned()
}

pub(crate) async fn status(State(state): State<AppState>) -> ApiResult<Json<Status>> {
    let (node, lightwalletd) =
        tokio::join!(state.0.rpc.chain_info(), state.0.wallet.latest_checkpoint());
    let node = node.ok();
    let lightwalletd = lightwalletd.ok();
    let wallet_sync = state.wallet_sync_status().await;
    let node_tip = node.as_ref().map(|chain| ChainCheckpoint {
        height: chain.blocks,
        hash: chain.bestblockhash.clone(),
    });
    let mut settlement = settlement_status(
        node_tip.as_ref(),
        lightwalletd.as_ref(),
        &wallet_sync,
        state.started(),
    );
    if settlement.settled && state.0.mining.chain_operations.try_read().is_err() {
        settlement.settled = false;
        settlement.settlement_blocker = Some("chain_operation");
    }
    Ok(Json(Status {
        instance: state.0.instance.clone(),
        node,
        account_count: usize::from(USER_ACCOUNT_COUNT),
        auto_mine: true,
        network: "Regtest",
        endpoints: PublicEndpoints {
            dashboard: public_dashboard_endpoint(
                std::env::var("THS_PUBLIC_DASHBOARD").ok().as_deref(),
            ),
            zakura_rpc: std::env::var("THS_PUBLIC_ZAKURA_RPC")
                .unwrap_or_else(|_| "http://127.0.0.1:18232".into()),
            lightwalletd: std::env::var("THS_PUBLIC_LIGHTWALLETD")
                .unwrap_or_else(|_| "http://127.0.0.1:9067".into()),
            p2p: std::env::var("THS_PUBLIC_P2P").unwrap_or_else(|_| "127.0.0.1:18233".into()),
        },
        wallet_sync,
        settlement,
    }))
}
pub(crate) async fn accounts(State(state): State<AppState>) -> ApiResult<Json<Vec<Account>>> {
    let mut accounts = state.accounts().await;
    accounts.retain(|account| account.id <= USER_ACCOUNT_COUNT);
    Ok(Json(accounts))
}

#[derive(Deserialize)]
pub(crate) struct SeedRequest {
    confirmation: String,
}
pub(crate) async fn seed(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<SeedRequest>,
) -> ApiResult<Json<Value>> {
    if req.confirmation != "I understand this seed is for regtest only" {
        return Err(ApiError::bad_request("exact confirmation phrase required"));
    }
    Ok(Json(
        json!({"seed_hex":state.0.store.seed()?,"warning":"Never send real funds to this development seed."}),
    ))
}

#[cfg(test)]
#[path = "../../tests/unit/api/status/tests.rs"]
pub(crate) mod tests;

#[derive(Deserialize)]
pub(crate) struct Page {
    limit: Option<u32>,
    idempotency_key: Option<String>,
}
pub(crate) async fn activity(
    State(state): State<AppState>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Json<Vec<Activity>>> {
    if let Some(key) = page.idempotency_key {
        require_key(&key)?;
        return Ok(Json(
            state.0.store.activity_for_key(&key)?.into_iter().collect(),
        ));
    }
    Ok(Json(state.0.store.activities(page.limit.unwrap_or(30))?))
}
