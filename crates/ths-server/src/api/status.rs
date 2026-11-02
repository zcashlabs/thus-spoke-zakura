use super::error::{ApiError, ApiJson, ApiQuery, ApiResult};
use super::payments::require_key;
use crate::state::AppState;
use crate::{
    db::{Account, Activity, USER_ACCOUNT_COUNT},
    rpc::{ChainCheckpoint, ChainInfo, NodeRpc},
    state::{WalletSyncStatus, wallet_matches_tip},
    wallet::{CanonicalActivity, CanonicalActivityIndex},
};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header::HOST, uri::Authority},
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

/// `ths run` lets Docker choose the dashboard's host port when the app container starts, so the
/// launcher can only configure port 0. The request's Host, already restricted to loopback by the
/// router, then names the port the caller actually reached. A Host without a nonzero port, such
/// as `localhost`, doesn't say which port that was, so the configured value is kept.
fn public_dashboard_endpoint(configured: Option<&str>, request: &HeaderMap) -> String {
    let configured = configured.unwrap_or("http://127.0.0.1:8080");
    let unassigned = configured
        .parse::<Uri>()
        .is_ok_and(|uri| uri.port_u16() == Some(0));
    let request_host = request.get(HOST).and_then(|host| host.to_str().ok());
    match request_host.and_then(|host| host.parse::<Authority>().ok()) {
        Some(host) if unassigned => match host.port_u16() {
            Some(port) if port != 0 => format!("http://{}:{port}", host.host()),
            _ => configured.to_owned(),
        },
        _ => configured.to_owned(),
    }
}

pub(crate) async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Status>> {
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
                &headers,
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
    account: Option<u8>,
    kind: Option<String>,
    status: Option<String>,
    before: Option<String>,
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

#[derive(Clone, Debug)]
struct ActivityCandidate {
    id: String,
    kind: String,
    status: String,
    affected_accounts: Vec<u8>,
    mined_height: Option<u32>,
    tx_index: Option<u32>,
    created_at: Option<String>,
    journal: Option<Activity>,
    canonical_txid: Option<String>,
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

fn add_public_source_pools(activity: &mut ActivityEntry, transaction: &Value) {
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
        activity.source_pool =
            (activity.source_pools.len() == 1).then(|| activity.source_pools[0].clone());
    }
}

async fn enrich_external_source_pools_within(
    rpc: &NodeRpc,
    activity: &mut [ActivityEntry],
    budget: Duration,
) {
    const CONCURRENCY: usize = 8;
    let deadline = Instant::now() + budget;
    let requests = activity
        .iter_mut()
        .enumerate()
        // Journal faucet rows already carry their known source pool. Only
        // canonical incoming transactions can need public-input discovery.
        .filter(|(_, entry)| entry.kind == "transaction" && entry.direction == "incoming")
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
    let mut activity = unified_activity_with_prepared(journal, canonical, &HashMap::new());
    activity.truncate(limit.min(100) as usize);
    activity
}

#[cfg(test)]
fn unified_activity_with_prepared(
    journal: Vec<Activity>,
    canonical: Vec<CanonicalActivity>,
    prepared_txids: &HashMap<String, String>,
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
    activity
}

#[cfg(test)]
fn order_activity_by_first_seen(
    activity: &mut [ActivityEntry],
    sequences: &HashMap<String, i64>,
) -> anyhow::Result<()> {
    if let Some(entry) = activity
        .iter()
        .find(|entry| !sequences.contains_key(&entry.id))
    {
        anyhow::bail!("activity {} has no first-seen sequence", entry.id);
    }
    activity.sort_by(|left, right| sequences[&right.id].cmp(&sequences[&left.id]));
    Ok(())
}

fn activity_candidates(
    journal: Vec<Activity>,
    canonical: Vec<CanonicalActivityIndex>,
    prepared_txids: &HashMap<String, String>,
) -> Vec<ActivityCandidate> {
    let mut canonical = canonical
        .into_iter()
        .map(|entry| (entry.txid.clone(), entry))
        .collect::<HashMap<_, _>>();
    let mut candidates = Vec::new();
    for mut journal in journal {
        if let Some(prepared_txid) = prepared_txids.get(&journal.id) {
            journal.txid.clone_from(prepared_txid);
        }
        let canonical = (!journal.txid.is_empty())
            .then(|| canonical.remove(&journal.txid))
            .flatten();
        let mut affected_accounts = canonical
            .as_ref()
            .map(|entry| entry.affected_accounts.clone())
            .unwrap_or_default();
        for account in [journal.from_account, journal.to_account]
            .into_iter()
            .flatten()
            .filter(|account| *account <= USER_ACCOUNT_COUNT)
        {
            if !affected_accounts.contains(&account) {
                affected_accounts.push(account);
            }
        }
        affected_accounts.sort_unstable();
        let mined_height = canonical.as_ref().and_then(|entry| entry.mined_height);
        let tx_index = canonical.as_ref().and_then(|entry| entry.tx_index);
        let canonical_txid = canonical.as_ref().map(|entry| entry.txid.clone());
        candidates.push(ActivityCandidate {
            id: journal.id.clone(),
            kind: journal.kind.clone(),
            status: if mined_height.is_some() {
                "confirmed".to_owned()
            } else {
                journal.status.clone()
            },
            affected_accounts,
            mined_height,
            tx_index,
            created_at: Some(journal.created_at.clone()),
            journal: Some(journal),
            canonical_txid,
        });
    }
    candidates.extend(canonical.into_values().map(|entry| {
        ActivityCandidate {
            id: format!("wallet:{}", entry.txid),
            kind: "transaction".to_owned(),
            status: if entry.mined_height.is_some() {
                "confirmed"
            } else {
                "pending"
            }
            .to_owned(),
            affected_accounts: entry.affected_accounts,
            mined_height: entry.mined_height,
            tx_index: entry.tx_index,
            created_at: entry.created_at,
            journal: None,
            canonical_txid: Some(entry.txid),
        }
    }));
    candidates.sort_by(|left, right| {
        let group = |entry: &ActivityCandidate| match (entry.mined_height, entry.status.as_str()) {
            (None, "confirmed") => 2,
            (None, _) => 0,
            (Some(_), _) => 1,
        };
        match group(left).cmp(&group(right)) {
            std::cmp::Ordering::Equal => match (left.mined_height, right.mined_height) {
                (Some(left_height), Some(right_height)) => right_height
                    .cmp(&left_height)
                    .then_with(|| right.tx_index.cmp(&left.tx_index)),
                _ => right.created_at.cmp(&left.created_at),
            },
            order => order,
        }
    });
    candidates
}

fn order_candidates_by_first_seen(
    activity: &mut [ActivityCandidate],
    sequences: &HashMap<String, i64>,
) -> anyhow::Result<()> {
    if let Some(entry) = activity
        .iter()
        .find(|entry| !sequences.contains_key(&entry.id))
    {
        anyhow::bail!("activity {} has no first-seen sequence", entry.id);
    }
    activity.sort_by(|left, right| sequences[&right.id].cmp(&sequences[&left.id]));
    Ok(())
}

const ACTIVITY_KINDS: [&str; 3] = ["send", "faucet", "transaction"];
const ACTIVITY_STATUSES: [&str; 7] = [
    "preparing",
    "prepared",
    "broadcast",
    "pending",
    "confirmed",
    "unconfirmed",
    "failed",
];

fn validate_activity_page(page: &Page) -> ApiResult<()> {
    if let Some(account) = page.account
        && !(1..=USER_ACCOUNT_COUNT).contains(&account)
    {
        return Err(ApiError::bad_request("account must be between 1 and 5"));
    }
    if let Some(kind) = page.kind.as_deref()
        && !ACTIVITY_KINDS.contains(&kind)
    {
        return Err(ApiError::bad_request(
            "kind must be send, faucet, or transaction",
        ));
    }
    if let Some(status) = page.status.as_deref()
        && !ACTIVITY_STATUSES.contains(&status)
    {
        return Err(ApiError::bad_request(
            "status must be preparing, prepared, broadcast, pending, confirmed, unconfirmed, or failed",
        ));
    }
    if page.before.as_deref() == Some("") {
        return Err(ApiError::bad_request("before must not be empty"));
    }
    Ok(())
}

#[cfg(test)]
fn activity_page(
    mut activity: Vec<ActivityEntry>,
    page: &Page,
) -> ApiResult<(Vec<ActivityEntry>, bool)> {
    validate_activity_page(page)?;

    activity.retain(|entry| {
        page.account.is_none_or(|account| {
            entry.from_accounts.contains(&account) || entry.to_accounts.contains(&account)
        }) && page.kind.as_deref().is_none_or(|kind| entry.kind == kind)
            && page
                .status
                .as_deref()
                .is_none_or(|status| entry.status == status)
    });

    let start = match page.before.as_deref() {
        Some(before) => activity
            .iter()
            .position(|entry| entry.id == before)
            .map(|position| position + 1)
            .ok_or_else(|| {
                ApiError::conflict("activity cursor is stale; reload from the first page")
            })?,
        None => 0,
    };
    let limit = page.limit.unwrap_or(30).min(100) as usize;
    let mut entries = activity
        .into_iter()
        .skip(start)
        .take(limit + 1)
        .collect::<Vec<_>>();
    let has_more = entries.len() > limit;
    entries.truncate(limit);
    Ok((entries, has_more))
}

fn candidate_page(
    mut activity: Vec<ActivityCandidate>,
    page: &Page,
) -> ApiResult<(Vec<ActivityCandidate>, bool)> {
    validate_activity_page(page)?;
    activity.retain(|entry| {
        page.account
            .is_none_or(|account| entry.affected_accounts.contains(&account))
            && page.kind.as_deref().is_none_or(|kind| entry.kind == kind)
            && page
                .status
                .as_deref()
                .is_none_or(|status| entry.status == status)
    });
    let start = match page.before.as_deref() {
        Some(before) => activity
            .iter()
            .position(|entry| entry.id == before)
            .map(|position| position + 1)
            .ok_or_else(|| {
                ApiError::conflict("activity cursor is stale; reload from the first page")
            })?,
        None => 0,
    };
    let limit = page.limit.unwrap_or(30).min(100) as usize;
    let mut entries = activity
        .into_iter()
        .skip(start)
        .take(limit + 1)
        .collect::<Vec<_>>();
    let has_more = entries.len() > limit;
    entries.truncate(limit);
    Ok((entries, has_more))
}

fn hydrate_candidates(
    candidates: Vec<ActivityCandidate>,
    canonical: Vec<CanonicalActivity>,
) -> ApiResult<Vec<ActivityEntry>> {
    let mut canonical = canonical
        .into_iter()
        .map(|entry| (entry.txid.clone(), entry))
        .collect::<HashMap<_, _>>();
    candidates
        .into_iter()
        .map(|candidate| {
            let hydrated = candidate
                .canonical_txid
                .as_ref()
                .and_then(|txid| canonical.remove(txid));
            match (candidate.journal, hydrated) {
                (Some(journal), Some(canonical)) => {
                    Ok(ActivityEntry::canonical(canonical).merge_journal(journal))
                }
                (Some(journal), None) if candidate.canonical_txid.is_none() => {
                    Ok(ActivityEntry::journal(journal))
                }
                (None, Some(canonical)) => Ok(ActivityEntry::canonical(canonical)),
                _ => Err(ApiError::conflict(
                    "activity changed while the page was loading; reload from the first page",
                )),
            }
        })
        .collect()
}

#[cfg(test)]
async fn enriched_activity_page(
    rpc: &NodeRpc,
    activity: Vec<ActivityEntry>,
    page: &Page,
    budget: Duration,
) -> ApiResult<(Vec<ActivityEntry>, bool)> {
    let (mut entries, has_more) = activity_page(activity, page)?;
    // External transparent-source discovery needs a Zakura RPC per incoming
    // transaction. It does not affect filtering or ordering, so keep that
    // network work proportional to the page the caller will actually receive.
    enrich_external_source_pools_within(rpc, &mut entries, budget).await;
    Ok((entries, has_more))
}

#[cfg(test)]
fn page_activity(activity: Vec<ActivityEntry>, page: &Page) -> ApiResult<Vec<ActivityEntry>> {
    activity_page(activity, page).map(|(entries, _)| entries)
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
    let journal = state.0.store.all_activities()?;
    let journal_ids = journal.iter().map(|entry| entry.id.clone()).collect();
    let (canonical, prepared_txids) = state.0.wallet.canonical_activity_index(journal_ids).await?;
    let mut candidates = activity_candidates(journal, canonical, &prepared_txids);
    let activity_ids = candidates
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<Vec<_>>();
    let sequences = state.0.store.activity_sequences(&activity_ids)?;
    order_candidates_by_first_seen(&mut candidates, &sequences)?;
    let (candidates, has_more) = candidate_page(candidates, &page)?;
    let txids = candidates
        .iter()
        .filter_map(|entry| entry.canonical_txid.clone())
        .collect::<HashSet<_>>();
    let canonical = if txids.is_empty() {
        Vec::new()
    } else {
        state.0.wallet.canonical_activity_by_txids(txids).await?
    };
    let mut entries = hydrate_candidates(candidates, canonical)?;
    enrich_external_source_pools_within(&state.0.rpc, &mut entries, Duration::from_secs(2)).await;
    let mut response = Json(entries).into_response();
    response.headers_mut().insert(
        "x-ths-has-more",
        HeaderValue::from_static(if has_more { "true" } else { "false" }),
    );
    Ok(response)
}
