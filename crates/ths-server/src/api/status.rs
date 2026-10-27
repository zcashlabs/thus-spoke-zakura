use super::error::{ApiError, ApiJson, ApiQuery, ApiResult};
use super::payments::require_key;
use crate::state::AppState;
use crate::{
    db::{Account, Activity, USER_ACCOUNT_COUNT},
    rpc::{ChainCheckpoint, ChainInfo, NodeRpc},
    state::{WalletSyncStatus, wallet_matches_tip},
    wallet::CanonicalActivity,
};
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
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
    /// The exact selected mode: `auto`, `interval`, `mempool`, or `manual`.
    mining_mode: &'static str,
    /// Legacy flag: true only when a payment operation mines its own block.
    auto_mine: bool,
    /// The interval in seconds when `mining_mode` is `interval`.
    block_time: Option<u64>,
    /// True when `mining_mode` is `mempool`.
    mine_on_mempool: bool,
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
    let mining = state.mining_config();
    Ok(Json(Status {
        instance: state.0.instance.clone(),
        node,
        account_count: usize::from(USER_ACCOUNT_COUNT),
        mining_mode: mining.mode.as_str(),
        auto_mine: mining.mode.auto_mine(),
        block_time: mining.block_time,
        mine_on_mempool: matches!(mining.mode, crate::mining::MiningMode::Mempool),
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

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ActivityEntry {
    id: String,
    kind: String,
    direction: String,
    from_account: Option<u8>,
    to_account: Option<u8>,
    from_accounts: Vec<u8>,
    to_accounts: Vec<u8>,
    to_address: Option<String>,
    recipient_addresses: Vec<String>,
    source_pool: Option<String>,
    destination_pool: Option<String>,
    source_pools: Vec<String>,
    destination_pools: Vec<String>,
    amount_zatoshi: u64,
    received_zatoshi: Option<u64>,
    spent_zatoshi: Option<u64>,
    net_zatoshi: Option<i64>,
    txid: String,
    block_hash: Option<String>,
    mined_height: Option<u32>,
    #[serde(skip)]
    tx_index: Option<u32>,
    confirmations: u32,
    status: String,
    created_at: Option<String>,
    memos: Vec<String>,
    last_error: Option<String>,
}

fn visible_account(account: Option<u8>) -> Option<u8> {
    account.filter(|id| *id <= USER_ACCOUNT_COUNT)
}

fn only_account(accounts: &[u8]) -> Option<u8> {
    match accounts {
        [account] => Some(*account),
        _ => None,
    }
}

fn activity_direction(kind: &str, from_accounts: &[u8], to_accounts: &[u8]) -> &'static str {
    if kind == "faucet" {
        "incoming"
    } else {
        match (from_accounts, to_accounts) {
            ([from], [to]) if from == to => "self",
            ([], []) => "unknown",
            ([], _) => "incoming",
            (_, []) => "outgoing",
            (_, _) => "internal",
        }
    }
}

impl ActivityEntry {
    fn journal(activity: Activity) -> Self {
        let from_account = visible_account(activity.from_account);
        let to_account = visible_account(activity.to_account);
        let from_accounts = from_account.into_iter().collect::<Vec<_>>();
        let to_accounts = to_account.into_iter().collect::<Vec<_>>();
        let direction = activity_direction(&activity.kind, &from_accounts, &to_accounts).to_owned();
        let recipient_addresses = activity.to_address.iter().cloned().collect();
        Self {
            id: activity.id,
            kind: activity.kind,
            direction,
            from_account,
            to_account,
            from_accounts,
            to_accounts,
            to_address: activity.to_address,
            recipient_addresses,
            source_pool: Some(activity.source_pool.clone()),
            destination_pool: Some(activity.destination_pool.clone()),
            source_pools: vec![activity.source_pool],
            destination_pools: vec![activity.destination_pool],
            amount_zatoshi: activity.amount_zatoshi,
            received_zatoshi: None,
            spent_zatoshi: None,
            net_zatoshi: None,
            txid: activity.txid,
            block_hash: activity.block_hash,
            mined_height: None,
            tx_index: None,
            confirmations: 0,
            status: activity.status,
            created_at: Some(activity.created_at),
            memos: Vec::new(),
            last_error: activity.last_error,
        }
    }

    fn canonical(activity: CanonicalActivity) -> Self {
        let recipient_addresses = activity.recipient_addresses;
        let to_address = activity
            .to_accounts
            .is_empty()
            .then(|| recipient_addresses.first().cloned())
            .flatten();
        Self {
            id: format!("wallet:{}", activity.txid),
            kind: "transaction".to_owned(),
            direction: activity.direction.to_owned(),
            from_account: activity.from_account,
            to_account: activity.to_account,
            from_accounts: activity.from_accounts,
            to_accounts: activity.to_accounts,
            to_address,
            recipient_addresses,
            source_pool: (activity.source_pools.len() == 1)
                .then(|| activity.source_pools[0].clone()),
            destination_pool: (activity.destination_pools.len() == 1)
                .then(|| activity.destination_pools[0].clone()),
            source_pools: activity.source_pools,
            destination_pools: activity.destination_pools,
            amount_zatoshi: activity.amount_zatoshi,
            received_zatoshi: Some(activity.received_zatoshi),
            spent_zatoshi: Some(activity.spent_zatoshi),
            net_zatoshi: Some(activity.net_zatoshi),
            txid: activity.txid,
            block_hash: activity.block_hash,
            mined_height: activity.mined_height,
            tx_index: activity.tx_index,
            confirmations: activity.confirmations,
            status: if activity.mined_height.is_some() {
                "confirmed"
            } else {
                "pending"
            }
            .to_owned(),
            created_at: activity.created_at,
            memos: activity.memos,
            last_error: None,
        }
    }

    fn merge_journal(mut self, journal: Activity) -> Self {
        self.id = journal.id;
        self.kind = journal.kind;
        if let Some(account) = visible_account(journal.from_account)
            && !self.from_accounts.contains(&account)
        {
            self.from_accounts.push(account);
            self.from_accounts.sort_unstable();
        }
        if let Some(account) = visible_account(journal.to_account)
            && !self.to_accounts.contains(&account)
        {
            self.to_accounts.push(account);
            self.to_accounts.sort_unstable();
        }
        self.from_account = only_account(&self.from_accounts);
        self.to_account = only_account(&self.to_accounts);
        self.direction =
            activity_direction(&self.kind, &self.from_accounts, &self.to_accounts).to_owned();
        self.to_address = if !self.to_accounts.is_empty() {
            None
        } else {
            journal.to_address.clone().or(self.to_address)
        };
        if let Some(address) = journal.to_address.as_ref()
            && !self.recipient_addresses.contains(address)
        {
            self.recipient_addresses.push(address.clone());
            self.recipient_addresses.sort();
        }
        self.source_pool = Some(journal.source_pool.clone());
        self.destination_pool = Some(journal.destination_pool.clone());
        self.source_pools = vec![journal.source_pool];
        self.destination_pools = vec![journal.destination_pool];
        self.amount_zatoshi = journal.amount_zatoshi;
        self.block_hash = self.block_hash.or(journal.block_hash);
        if self.mined_height.is_none() {
            self.status = journal.status;
            self.last_error = journal.last_error;
        }
        self.created_at = Some(journal.created_at);
        self
    }
}

fn add_public_source_pools(activity: &mut CanonicalActivity, transaction: &Value) {
    let has_transparent_inputs = transaction
        .get("vin")
        .and_then(Value::as_array)
        .is_some_and(|inputs| {
            inputs.iter().any(|input| {
                input.get("coinbase").is_none()
                    && input.get("txid").and_then(Value::as_str).is_some()
            })
        });
    if has_transparent_inputs
        && !activity
            .source_pools
            .iter()
            .any(|pool| pool == "transparent")
    {
        activity.source_pools.push("transparent".to_owned());
        activity.source_pools.sort();
    }
}

async fn enrich_external_source_pools(state: &AppState, activity: &mut [CanonicalActivity]) {
    enrich_external_source_pools_within(&state.0.rpc, activity, Duration::from_secs(2)).await;
}

async fn enrich_external_source_pools_within(
    rpc: &NodeRpc,
    activity: &mut [CanonicalActivity],
    budget: Duration,
) {
    const CONCURRENCY: usize = 8;
    let deadline = Instant::now() + budget;
    let requests = activity
        .iter_mut()
        .enumerate()
        .filter(|(_, entry)| entry.direction == "incoming")
        .map(|(index, entry)| (index, entry.txid.clone()))
        .collect::<Vec<_>>();
    let mut responses = stream::iter(requests)
        .map(|(index, txid)| async move {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let result = if remaining.is_zero() {
                None
            } else {
                Some(rpc.transaction_within(&txid, remaining).await)
            };
            (index, txid, result)
        })
        .buffer_unordered(CONCURRENCY);
    while let Some((index, txid, result)) = responses.next().await {
        match result {
            Some(Ok(transaction)) => add_public_source_pools(&mut activity[index], &transaction),
            Some(Err(error)) => tracing::warn!(
                error = %format_args!("{error:#}"),
                %txid,
                "reading public activity inputs failed"
            ),
            None => {}
        }
    }
}

#[cfg(test)]
fn unified_activity(
    journal: Vec<Activity>,
    canonical: Vec<CanonicalActivity>,
    limit: u32,
) -> Vec<ActivityEntry> {
    unified_activity_with_prepared(journal, canonical, &HashMap::new(), limit)
}

fn unified_activity_with_prepared(
    journal: Vec<Activity>,
    canonical: Vec<CanonicalActivity>,
    prepared_txids: &HashMap<String, String>,
    limit: u32,
) -> Vec<ActivityEntry> {
    let mut transaction_ids = HashSet::new();
    let mut canonical = canonical
        .into_iter()
        .filter(|entry| transaction_ids.insert(entry.txid.clone()))
        .map(Some)
        .collect::<Vec<_>>();
    let mut activity = Vec::new();
    for mut journal in journal {
        if let Some(prepared_txid) = prepared_txids.get(&journal.id) {
            journal.txid.clone_from(prepared_txid);
        }
        let matched = (!journal.txid.is_empty()).then(|| {
            canonical.iter().position(|candidate| {
                candidate
                    .as_ref()
                    .is_some_and(|candidate| candidate.txid == journal.txid)
            })
        });
        let entry = if let Some(entry) = matched.flatten().and_then(|index| canonical[index].take())
        {
            ActivityEntry::canonical(entry).merge_journal(journal)
        } else {
            ActivityEntry::journal(journal)
        };
        activity.push(entry);
    }
    activity.extend(
        canonical
            .into_iter()
            .flatten()
            .map(ActivityEntry::canonical),
    );
    activity.sort_by(|left, right| {
        let group = |entry: &ActivityEntry| match (entry.mined_height, entry.status.as_str()) {
            (None, "confirmed") => 2,
            (None, _) => 0,
            (Some(_), _) => 1,
        };
        match group(left).cmp(&group(right)) {
            std::cmp::Ordering::Equal => match (left.mined_height, right.mined_height) {
                // Block position is the canonical order. Regtest block timestamps are
                // deterministic and begin in 2011, so they must not order activity.
                (Some(left_height), Some(right_height)) => right_height
                    .cmp(&left_height)
                    .then_with(|| right.tx_index.cmp(&left.tx_index)),
                // Local timestamps order unresolved work and confirmed journal
                // fallbacks separately. A confirmed fallback has no canonical height
                // only because it fell outside the bounded wallet-history window; it
                // must not be mistaken for pending work.
                _ => right.created_at.cmp(&left.created_at),
            },
            order => order,
        }
    });
    activity.truncate(limit.min(100) as usize);
    activity
}

pub(crate) async fn activity(
    State(state): State<AppState>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Response> {
    if let Some(key) = page.idempotency_key {
        require_key(&key)?;
        return Ok(Json(
            state
                .0
                .store
                .activity_for_key(&key)?
                .into_iter()
                .collect::<Vec<_>>(),
        )
        .into_response());
    }
    let limit = page.limit.unwrap_or(30).min(100);
    let journal = state.0.store.activities(100)?;
    let journal_ids = journal.iter().map(|entry| entry.id.clone()).collect();
    let (mut canonical, prepared_txids) =
        state.0.wallet.canonical_activity(100, journal_ids).await?;
    enrich_external_source_pools(&state, &mut canonical).await;
    Ok(Json(unified_activity_with_prepared(
        journal,
        canonical,
        &prepared_txids,
        limit,
    ))
    .into_response())
}
