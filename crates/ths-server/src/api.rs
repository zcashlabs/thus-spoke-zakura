use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    handler::HandlerWithoutStateExt,
    http::{HeaderMap, StatusCode, Uri, header::HOST},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, RwLock, broadcast},
    time::Instant,
};
use tower_http::{services::ServeDir, trace::TraceLayer};
use zcash_keys::{address::Address, encoding::AddressCodec};
use zcash_protocol::{consensus::COINBASE_MATURITY_BLOCKS, memo::MemoBytes, value::MAX_MONEY};

use crate::{
    db::{
        Account, Activity, AddressFaucet, IdempotencyConflict, PreparedTransaction, Store,
        TREASURY_ACCOUNT_ID, USER_ACCOUNT_COUNT, ZATOSHIS_PER_ZEC,
    },
    mining::{
        Admission, MiningAdmissionError, MiningCoordinator, MiningJob, MiningRuntime, MiningState,
        validate_idempotency_key,
    },
    rpc::{ChainCheckpoint, ChainInfo, NodeRpc, RpcError},
    wallet::{
        PaymentError, PreparedPayment, RealWallet, SendQuote, WALLET_BIRTHDAY_HEIGHT,
        regtest_network,
    },
};

#[derive(Clone)]
pub struct AppState(Arc<Inner>);
struct Inner {
    mining: Arc<MiningCoordinator>,
    #[cfg(test)]
    mining_runtime: Option<Arc<dyn MiningRuntime>>,
    store: Store,
    wallet: RealWallet,
    rpc: NodeRpc,
    instance: String,
    events: broadcast::Sender<String>,
    payments: Mutex<()>,
    wallet_sync: Mutex<()>,
    treasury_replenishment: Mutex<()>,
    wallet_snapshot: RwLock<WalletSnapshot>,
}

#[derive(Clone)]
struct WalletSnapshot {
    accounts: Vec<Account>,
    status: WalletSyncStatus,
}

#[derive(Clone, Serialize)]
struct WalletSyncStatus {
    state: &'static str,
    fully_scanned_height: Option<u64>,
    fully_scanned_hash: Option<String>,
    observed_height: Option<u64>,
    observed_hash: Option<String>,
    last_success_at: Option<u64>,
    error: Option<String>,
}

impl AppState {
    pub fn new(store: Store, wallet: RealWallet, rpc: String, instance: String) -> Self {
        let (events, _) = broadcast::channel(128);
        let accounts = store.accounts().unwrap_or_default();
        Self(Arc::new(Inner {
            mining: Arc::new(MiningCoordinator::new()),
            #[cfg(test)]
            mining_runtime: None,
            store,
            wallet,
            rpc: NodeRpc::new(rpc),
            instance,
            events,
            payments: Mutex::new(()),
            wallet_sync: Mutex::new(()),
            treasury_replenishment: Mutex::new(()),
            wallet_snapshot: RwLock::new(WalletSnapshot {
                accounts,
                status: WalletSyncStatus {
                    state: "syncing",
                    fully_scanned_height: None,
                    fully_scanned_hash: None,
                    observed_height: None,
                    observed_hash: None,
                    last_success_at: None,
                    error: None,
                },
            }),
        }))
    }

    async fn accounts(&self) -> Vec<Account> {
        self.0.wallet_snapshot.read().await.accounts.clone()
    }

    async fn wallet_sync_status(&self) -> WalletSyncStatus {
        self.0.wallet_snapshot.read().await.status.clone()
    }

    async fn synchronize_wallet(&self, target_height: Option<u64>) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(120);
        let _guard = tokio::time::timeout_at(deadline, self.0.wallet_sync.lock()).await?;

        {
            let mut snapshot = self.0.wallet_snapshot.write().await;
            snapshot.status.state = "syncing";
            snapshot.status.error = None;
        }
        notify(self, "sync");

        let result = async {
            if let Some(target) = target_height {
                self.0
                    .wallet
                    .wait_for_height(target, deadline.saturating_duration_since(Instant::now()))
                    .await?;
            }
            let target = crate::reconcile::within_deadline(
                deadline,
                "reading the reconciliation target",
                self.0.rpc.checkpoint(),
            )
            .await?;
            if let Some(height) = target_height {
                anyhow::ensure!(
                    target.height >= height,
                    "mined height is no longer canonical"
                );
            }
            crate::reconcile::sync_wallet(&self.0.wallet, &self.0.rpc, &target, deadline).await?;
            self.refresh_wallet_snapshot(&target).await
        }
        .await;

        if let Err(error) = &result {
            let mut snapshot = self.0.wallet_snapshot.write().await;
            snapshot.status.state = "error";
            snapshot.status.error = Some(error.to_string());
            notify(self, "sync");
        }
        result
    }

    async fn synchronize_latest(&self) -> anyhow::Result<()> {
        self.synchronize_wallet(None).await
    }

    async fn refresh_wallet_snapshot(&self, target: &ChainCheckpoint) -> anyhow::Result<()> {
        let mut accounts = self.0.store.accounts()?;
        self.0.wallet.apply_balances(&mut accounts).await?;
        let scanned = self.0.wallet.scanned_checkpoint().await?;
        let rpc = &self.0.rpc;
        let observed = rpc.checkpoint().await?;
        let publishable = scan_is_publishable(scanned.as_ref(), target, &observed, |height| {
            rpc.block_hash(height)
        })
        .await?;
        anyhow::ensure!(
            publishable,
            "chain checkpoint changed before wallet publication"
        );
        let changed = self.0.wallet_snapshot.read().await.accounts != accounts;
        {
            let mut snapshot = self.0.wallet_snapshot.write().await;
            snapshot.accounts = accounts;
            snapshot.status = WalletSyncStatus {
                state: "ready",
                fully_scanned_height: scanned.as_ref().map(|checkpoint| checkpoint.height),
                fully_scanned_hash: scanned.as_ref().map(|checkpoint| checkpoint.hash.clone()),
                observed_height: Some(observed.height),
                observed_hash: Some(observed.hash),
                last_success_at: Some(now_unix()),
                error: None,
            };
        }
        notify(self, "sync");
        if changed {
            notify(self, "wallet");
        }
        reconcile_unconfirmed(self).await?;
        Ok(())
    }

    async fn sync_if_chain_advanced(&self) -> anyhow::Result<()> {
        let observed = self.0.rpc.checkpoint().await?;
        let status = self.0.wallet_snapshot.read().await.status.clone();
        let settled = status.state == "ready"
            && ((status.fully_scanned_height == Some(observed.height)
                && status.fully_scanned_hash.as_deref() == Some(&observed.hash))
                || (observed.height < u64::from(WALLET_BIRTHDAY_HEIGHT)
                    && status.fully_scanned_height.is_none()
                    && status.observed_height == Some(observed.height)
                    && status.observed_hash.as_deref() == Some(&observed.hash)));
        {
            let mut snapshot = self.0.wallet_snapshot.write().await;
            snapshot.status.observed_height = Some(observed.height);
            snapshot.status.observed_hash = Some(observed.hash.clone());
        }
        if !settled {
            self.synchronize_wallet(Some(observed.height)).await?;
            notify(self, "chain");
        }
        Ok(())
    }
}

/// a scan that reached its target stays publishable after other requests mine past it, as long as
/// the node still has the scanned block.
async fn scan_is_publishable<F, Fut>(
    scanned: Option<&ChainCheckpoint>,
    target: &ChainCheckpoint,
    observed: &ChainCheckpoint,
    canonical_hash: F,
) -> anyhow::Result<bool>
where
    F: FnOnce(u32) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<String>>,
{
    Ok(match scanned {
        Some(scanned) if scanned == observed => scanned.height >= target.height,
        Some(scanned) if (target.height..observed.height).contains(&scanned.height) => {
            canonical_hash(u32::try_from(scanned.height)?).await? == scanned.hash
        }
        Some(_) => false,
        None => observed.height < u64::from(WALLET_BIRTHDAY_HEIGHT),
    })
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub async fn wallet_sync_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if let Err(error) = state.sync_if_chain_advanced().await {
            tracing::warn!(
                error = %format_args!("{error:#}"),
                "background wallet synchronization failed"
            );
            let mut snapshot = state.0.wallet_snapshot.write().await;
            snapshot.status.state = "error";
            snapshot.status.error = Some(error.to_string());
            drop(snapshot);
            notify(&state, "sync");
        }
    }
}

/// Serves the dashboard shell for client-side routes only.
///
/// The dashboard is a single-page app, so an unknown path is usually a route
/// like `/explorer/block/42` and must return the shell with `200`. It is not
/// a blanket catch-all: an unknown `/api/` path is a genuine 404, and a
/// missing asset must stay a 404 rather than returning HTML that the browser
/// would then try to parse as JavaScript or CSS.
async fn spa_fallback(uri: Uri, index: Arc<Option<String>>) -> Response {
    let path = uri.path();
    let looks_like_a_file = path
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'));

    if path.starts_with("/api/") || looks_like_a_file {
        return ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("{path} does not exist"),
        }
        .into_response();
    }

    match index.as_ref() {
        Some(html) => Html(html.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, "dashboard assets are not installed").into_response(),
    }
}

/// Static assets plus the single-page fallback.
fn dashboard_router(static_dir: PathBuf) -> Router {
    // Read once: the shell is small and immutable for the life of the process.
    let index_html = Arc::new(std::fs::read_to_string(static_dir.join("index.html")).ok());
    Router::new().fallback_service(
        // `not_found_service` would wrap the fallback in `SetStatus(404)`,
        // which renders correctly but reports every deep link as missing.
        ServeDir::new(static_dir)
            .fallback((move |uri: Uri| spa_fallback(uri, Arc::clone(&index_html))).into_service()),
    )
}

pub fn router(state: AppState) -> Router {
    let static_dir = std::env::var("THS_WEB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("web/dist"));
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/status", get(status))
        .route("/api/v1/accounts", get(accounts))
        .route("/api/v1/activity", get(activity))
        .route("/api/v1/send", post(send))
        .route("/api/v1/send/quote", post(send_quote))
        .route("/api/v1/faucet", post(faucet))
        .route("/api/v1/faucet/address", post(faucet_address))
        .route("/api/v1/mine", post(mine))
        .route(
            "/api/v1/mining/jobs",
            get(latest_mining_job).post(start_mining_job),
        )
        .route("/api/v1/mining/jobs/{id}", get(mining_job))
        .route("/api/v1/dev/seed", post(seed))
        .route("/api/v1/blocks", get(blocks))
        .route("/api/v1/blocks/{id}", get(block))
        .route("/api/v1/transactions/{txid}", get(transaction))
        .route("/api/v1/mempool", get(mempool))
        .route("/api/v1/addresses/{address}", get(address))
        .route("/api/v1/search", get(search))
        .route("/api/v1/events", get(events))
        .fallback_service(dashboard_router(static_dir))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn dependencies_ready(state: &AppState) -> anyhow::Result<()> {
    state.0.rpc.chain_info().await?;
    state.synchronize_latest().await?;
    Ok(())
}

async fn health(State(state): State<AppState>) -> Response {
    let node = state.0.rpc.chain_info().await.ok();
    let wallet = state.wallet_sync_status().await;
    let status = if node.is_some() && wallet.last_success_at.is_some() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({"ok": node.is_some() && wallet.last_success_at.is_some(), "instance": state.0.instance, "node": node, "wallet_sync": wallet})),
    )
        .into_response()
}

#[derive(Serialize)]
struct Status {
    instance: String,
    node: Option<ChainInfo>,
    account_count: usize,
    auto_mine: bool,
    network: &'static str,
    endpoints: PublicEndpoints,
    wallet_sync: WalletSyncStatus,
}

#[derive(Serialize)]
struct PublicEndpoints {
    dashboard: String,
    zakura_rpc: String,
    lightwalletd: String,
    p2p: String,
}

async fn status(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Status>> {
    let dashboard_host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1:8080");
    Ok(Json(Status {
        instance: state.0.instance.clone(),
        node: state.0.rpc.chain_info().await.ok(),
        account_count: usize::from(USER_ACCOUNT_COUNT),
        auto_mine: true,
        network: "Regtest",
        endpoints: PublicEndpoints {
            dashboard: format!("http://{dashboard_host}"),
            zakura_rpc: std::env::var("THS_PUBLIC_ZAKURA_RPC")
                .unwrap_or_else(|_| "http://127.0.0.1:18232".into()),
            lightwalletd: std::env::var("THS_PUBLIC_LIGHTWALLETD")
                .unwrap_or_else(|_| "http://127.0.0.1:9067".into()),
            p2p: std::env::var("THS_PUBLIC_P2P").unwrap_or_else(|_| "127.0.0.1:18233".into()),
        },
        wallet_sync: state.wallet_sync_status().await,
    }))
}
async fn accounts(State(state): State<AppState>) -> ApiResult<Json<Vec<Account>>> {
    let mut accounts = state.accounts().await;
    accounts.retain(|account| account.id <= USER_ACCOUNT_COUNT);
    Ok(Json(accounts))
}

#[derive(Deserialize)]
struct Page {
    limit: Option<u32>,
}
async fn activity(
    State(state): State<AppState>,
    Query(page): Query<Page>,
) -> ApiResult<Json<Vec<Activity>>> {
    Ok(Json(state.0.store.activities(page.limit.unwrap_or(30))?))
}

#[derive(Deserialize)]
struct SendRequest {
    from_account: u8,
    to_account: u8,
    source_pool: String,
    destination_pool: String,
    amount_zatoshi: u64,
    idempotency_key: String,
    #[serde(default)]
    memo: Option<String>,
}
async fn send(
    State(state): State<AppState>,
    Json(req): Json<SendRequest>,
) -> ApiResult<Json<Activity>> {
    let memo = validate_send(&req)?;
    let _payment = state.0.payments.lock().await;
    let mut pending = state.0.store.claim_transfer(
        req.from_account,
        req.to_account,
        &req.source_pool,
        &req.destination_pool,
        req.amount_zatoshi,
        &req.idempotency_key,
        req.memo.as_deref(),
    )?;
    loop {
        match pending.status.as_str() {
            "confirmed" => return Ok(Json(pending)),
            "prepared" | "broadcast" => {
                match submit_prepared(&state.0.store, &state, &pending).await? {
                    PreparedSubmission::Broadcast(activity) => {
                        return Ok(Json(confirm_after_mining(&state, activity).await?));
                    }
                    PreparedSubmission::Expired(activity) => pending = activity,
                }
            }
            "preparing" => {
                let prepared = async {
                    state.synchronize_latest().await?;
                    let destination = state.0.store.account(req.to_account)?;
                    let address = match req.destination_pool.as_str() {
                        "transparent" => destination.transparent_address,
                        "ironwood" => destination.unified_address,
                        _ => unreachable!("claim_transfer validates destination_pool"),
                    };
                    state
                        .0
                        .wallet
                        .prepare(
                            Some(&pending.id),
                            &state.0.store.seed()?,
                            req.from_account,
                            &req.source_pool,
                            &address,
                            req.amount_zatoshi,
                            memo.clone(),
                        )
                        .await
                }
                .await;
                let prepared = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        discard_unprepared_claim(&state, &pending.id).await?;
                        return Err(error.into());
                    }
                };
                pending = state.0.store.record_prepared(
                    &pending.id,
                    &prepared.txid,
                    &prepared.raw_transaction,
                    prepared.expiry_height,
                )?;
            }
            status => {
                return Err(anyhow::anyhow!("payment has unsupported status {status}").into());
            }
        }
    }
}

#[derive(Deserialize)]
struct SendQuoteRequest {
    from_account: u8,
    source_pool: String,
    destination_pool: String,
}
async fn send_quote(
    State(state): State<AppState>,
    Json(req): Json<SendQuoteRequest>,
) -> ApiResult<Json<SendQuote>> {
    require_user_account(req.from_account)?;
    require_pool(&req.source_pool, "source_pool")?;
    require_pool(&req.destination_pool, "destination_pool")?;
    // The fee depends only on the destination's receiver kinds, which every
    // account shares, so the source account's own address quotes the same fee.
    let destination = state.0.store.account(req.from_account)?;
    let address = if req.destination_pool == "transparent" {
        destination.transparent_address
    } else {
        destination.unified_address
    };
    Ok(Json(
        state
            .0
            .wallet
            .send_quote(req.from_account, &req.source_pool, &address)
            .await?,
    ))
}

#[derive(Deserialize)]
struct FaucetRequest {
    account_id: u8,
    pool: String,
    amount_zatoshi: u64,
    idempotency_key: String,
}
async fn faucet(
    State(state): State<AppState>,
    Json(req): Json<FaucetRequest>,
) -> ApiResult<Json<Activity>> {
    require_key(&req.idempotency_key)?;
    require_user_account(req.account_id)?;
    if req.amount_zatoshi > 5 * ZATOSHIS_PER_ZEC {
        return Err(ApiError::bad_request(
            "a faucet request is limited to 5 ZEC",
        ));
    }
    if req.amount_zatoshi == 0 {
        return Err(ApiError::bad_request("amount must be greater than zero"));
    }
    require_pool(&req.pool, "pool")?;
    Ok(Json(
        fund_from_treasury(
            &state,
            req.account_id,
            &req.pool,
            req.amount_zatoshi,
            &req.idempotency_key,
        )
        .await?,
    ))
}

#[derive(Deserialize)]
struct FaucetAddressRequest {
    address: String,
    amount_zatoshi: u64,
    #[serde(default)]
    idempotency_key: String,
}

#[derive(Serialize)]
struct FaucetAddressResponse {
    address: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    status: String,
}

fn internal_faucet_destination(
    store: &Store,
    address: &str,
) -> anyhow::Result<Option<(u8, &'static str)>> {
    for account in store.accounts()? {
        if !(1..=USER_ACCOUNT_COUNT).contains(&account.id) {
            continue;
        }
        if account.transparent_address == address {
            return Ok(Some((account.id, "transparent")));
        }
        if account.unified_address == address {
            return Ok(Some((account.id, "ironwood")));
        }
    }
    Ok(None)
}

fn faucet_address_response(
    req: FaucetAddressRequest,
    activity: Activity,
) -> anyhow::Result<FaucetAddressResponse> {
    let block_hash = activity.block_hash.filter(|hash| !hash.is_empty());
    if activity.status == "confirmed" {
        anyhow::ensure!(
            block_hash.is_some(),
            "faucet transaction was not included in a block"
        );
    }
    Ok(FaucetAddressResponse {
        address: req.address,
        amount_zatoshi: req.amount_zatoshi,
        txid: activity.txid,
        block_hash,
        status: activity.status,
    })
}

#[async_trait::async_trait]
trait AddressFaucetRuntime: Sync {
    async fn fund_internal(
        &self,
        account_id: u8,
        pool: &str,
        amount_zatoshi: u64,
        key: &str,
    ) -> anyhow::Result<Activity>;
    async fn fund_external(
        &self,
        req: FaucetAddressRequest,
    ) -> anyhow::Result<FaucetAddressResponse>;
}

#[async_trait::async_trait]
impl AddressFaucetRuntime for AppState {
    async fn fund_internal(
        &self,
        account_id: u8,
        pool: &str,
        amount_zatoshi: u64,
        key: &str,
    ) -> anyhow::Result<Activity> {
        fund_from_treasury(self, account_id, pool, amount_zatoshi, key).await
    }

    async fn fund_external(
        &self,
        req: FaucetAddressRequest,
    ) -> anyhow::Result<FaucetAddressResponse> {
        let payment = fund_address_from_treasury(
            self,
            &req.address,
            req.amount_zatoshi,
            &req.idempotency_key,
        )
        .await?;
        Ok(FaucetAddressResponse {
            address: payment.address,
            amount_zatoshi: payment.amount_zatoshi,
            txid: payment.txid,
            block_hash: payment.block_hash,
            status: payment.status,
        })
    }
}

async fn execute_address_faucet<R: AddressFaucetRuntime>(
    store: &Store,
    runtime: &R,
    req: FaucetAddressRequest,
) -> ApiResult<Json<FaucetAddressResponse>> {
    require_key(&req.idempotency_key)?;
    require_faucet_address(&req.address)?;
    if req.amount_zatoshi == 0 || req.amount_zatoshi > 5 * ZATOSHIS_PER_ZEC {
        return Err(ApiError::bad_request(
            "amount must be greater than zero and no more than 5 ZEC",
        ));
    }
    if store
        .address_faucet_for_key(&req.idempotency_key)?
        .is_some()
    {
        return Ok(Json(runtime.fund_external(req).await?));
    }
    if let Some((account_id, pool)) = internal_faucet_destination(store, &req.address)? {
        let activity = runtime
            .fund_internal(account_id, pool, req.amount_zatoshi, &req.idempotency_key)
            .await?;
        Ok(Json(faucet_address_response(req, activity)?))
    } else {
        Ok(Json(runtime.fund_external(req).await?))
    }
}

async fn faucet_address(
    State(state): State<AppState>,
    Json(req): Json<FaucetAddressRequest>,
) -> ApiResult<Json<FaucetAddressResponse>> {
    execute_address_faucet(&state.0.store, &state, req).await
}

async fn fund_address_from_treasury(
    state: &AppState,
    address: &str,
    amount_zatoshi: u64,
    idempotency_key: &str,
) -> anyhow::Result<AddressFaucet> {
    let _payment = state.0.payments.lock().await;
    let mut pending =
        state
            .0
            .store
            .claim_address_faucet(address, amount_zatoshi, idempotency_key)?;
    loop {
        match pending.status.as_str() {
            "confirmed" => return Ok(pending),
            "prepared" | "broadcast" => {
                match submit_address_prepared(&state.0.store, state, &pending).await? {
                    AddressSubmission::Broadcast(payment) => {
                        pending = payment;
                        break;
                    }
                    AddressSubmission::Expired(payment) => pending = payment,
                }
            }
            "preparing" => {
                let prepared = async {
                    state.synchronize_latest().await?;
                    let seed = state.0.store.seed()?;
                    let treasury = state.0.store.account(TREASURY_ACCOUNT_ID)?;
                    let _replenishment = state.0.treasury_replenishment.lock().await;
                    prepare_with_replenishment(
                        state,
                        Some(&pending.id),
                        &seed,
                        &treasury,
                        address,
                        amount_zatoshi,
                    )
                    .await
                }
                .await;
                let prepared = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        if !state.0.wallet.has_prepared(&pending.id).await? {
                            state.0.store.discard_address_preparing(&pending.id)?;
                        }
                        return Err(error);
                    }
                };
                pending = state.0.store.record_address_prepared(
                    &pending.id,
                    &prepared.txid,
                    &prepared.raw_transaction,
                    prepared.expiry_height,
                )?;
            }
            status => anyhow::bail!("address faucet has unsupported status {status}"),
        }
    }
    confirm_address_after_mining(state, pending).await
}

async fn confirm_address_after_mining(
    state: &AppState,
    pending: AddressFaucet,
) -> anyhow::Result<AddressFaucet> {
    let check = |payment: AddressFaucet, tx: Value| -> anyhow::Result<AddressFaucet> {
        match confirmed_block_hash(&tx) {
            Some(hash) => state
                .0
                .store
                .confirm_address(&payment.id, &payment.txid, hash),
            None => Ok(payment),
        }
    };
    let pending = match state.0.rpc.transaction(&pending.txid).await {
        Ok(tx) => check(pending, tx)?,
        Err(error) => {
            tracing::warn!(
                error = %format_args!("{error:#}"),
                txid = %pending.txid,
                "could not check address faucet confirmation"
            );
            pending
        }
    };
    if pending.status == "confirmed" {
        return Ok(pending);
    }
    if let Err(error) = mine_and_sync(state, 1).await {
        tracing::warn!(
            error = %format_args!("{error:#}"),
            payment = %pending.id,
            "address faucet recorded but auto-mine failed"
        );
        return Ok(pending);
    }
    match state.0.rpc.transaction(&pending.txid).await {
        Ok(tx) => check(pending, tx),
        Err(error) => {
            tracing::warn!(
                error = %format_args!("{error:#}"),
                txid = %pending.txid,
                "could not check address faucet confirmation"
            );
            Ok(pending)
        }
    }
}

async fn fund_from_treasury(
    state: &AppState,
    account_id: u8,
    pool: &str,
    amount_zatoshi: u64,
    idempotency_key: &str,
) -> anyhow::Result<Activity> {
    let _payment = state.0.payments.lock().await;
    let mut pending =
        state
            .0
            .store
            .claim_faucet(account_id, pool, amount_zatoshi, idempotency_key)?;
    loop {
        match pending.status.as_str() {
            "confirmed" => return Ok(pending),
            "prepared" | "broadcast" => {
                match submit_prepared(&state.0.store, state, &pending).await? {
                    PreparedSubmission::Broadcast(activity) => {
                        pending = activity;
                        break;
                    }
                    PreparedSubmission::Expired(activity) => pending = activity,
                }
            }
            "preparing" => {
                let prepared = async {
                    let destination = state.0.store.account(account_id)?;
                    let address = match pool {
                        "transparent" => destination.transparent_address,
                        "ironwood" => destination.unified_address,
                        _ => unreachable!("claim_faucet validates pool"),
                    };
                    state.synchronize_latest().await?;
                    let seed = state.0.store.seed()?;
                    // SDK proposals check spendability and the actual fee before construction. Total
                    // balances include pending change and cannot decide whether this request is fundable.
                    let treasury = state.0.store.account(TREASURY_ACCOUNT_ID)?;
                    let _replenishment = state.0.treasury_replenishment.lock().await;
                    prepare_with_replenishment(
                        state,
                        Some(&pending.id),
                        &seed,
                        &treasury,
                        &address,
                        amount_zatoshi,
                    )
                    .await
                }
                .await;
                let prepared = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        discard_unprepared_claim(state, &pending.id).await?;
                        return Err(error);
                    }
                };
                pending = state.0.store.record_prepared(
                    &pending.id,
                    &prepared.txid,
                    &prepared.raw_transaction,
                    prepared.expiry_height,
                )?;
            }
            status => anyhow::bail!("payment has unsupported status {status}"),
        }
    }
    confirm_after_mining(state, pending).await
}

#[async_trait::async_trait]
trait PaymentSubmitter: Sync {
    async fn transaction_known(&self, txid: &str) -> anyhow::Result<bool>;
    async fn chain_height(&self) -> anyhow::Result<u64>;
    async fn broadcast(&self, raw_transaction: &[u8]) -> anyhow::Result<()>;
    async fn recover_prepared(&self, txid: &str) -> anyhow::Result<Option<PreparedPayment>>;
}

#[async_trait::async_trait]
impl PaymentSubmitter for AppState {
    async fn transaction_known(&self, txid: &str) -> anyhow::Result<bool> {
        self.0.rpc.transaction_known(txid).await
    }

    async fn chain_height(&self) -> anyhow::Result<u64> {
        Ok(self.0.rpc.chain_info().await?.blocks)
    }

    async fn broadcast(&self, raw_transaction: &[u8]) -> anyhow::Result<()> {
        self.0.wallet.broadcast(raw_transaction).await
    }

    async fn recover_prepared(&self, txid: &str) -> anyhow::Result<Option<PreparedPayment>> {
        self.0.wallet.recover_prepared(txid).await
    }
}

async fn discard_unprepared_claim(state: &AppState, id: &str) -> anyhow::Result<()> {
    if !state.0.wallet.has_prepared(id).await? {
        state.0.store.discard_preparing(id)?;
    }
    Ok(())
}

async fn submit_prepared<R: PaymentSubmitter>(
    store: &Store,
    runtime: &R,
    activity: &Activity,
) -> anyhow::Result<PreparedSubmission> {
    match submit_saved_prepared(store, runtime, &activity.id, &activity.txid, |recovered| {
        store.record_prepared(
            &activity.id,
            &activity.txid,
            &recovered.raw_transaction,
            recovered.expiry_height,
        )?;
        Ok(())
    })
    .await?
    {
        SavedSubmission::Broadcast => Ok(PreparedSubmission::Broadcast(
            store.mark_broadcast(&activity.id, &activity.txid)?,
        )),
        SavedSubmission::Expired => Ok(PreparedSubmission::Expired(
            store.reset_for_retry(&activity.id, &activity.txid)?,
        )),
    }
}

enum AddressSubmission {
    Broadcast(AddressFaucet),
    Expired(AddressFaucet),
}

async fn submit_address_prepared<R: PaymentSubmitter>(
    store: &Store,
    runtime: &R,
    payment: &AddressFaucet,
) -> anyhow::Result<AddressSubmission> {
    match submit_saved_prepared(store, runtime, &payment.id, &payment.txid, |recovered| {
        store.record_address_prepared(
            &payment.id,
            &payment.txid,
            &recovered.raw_transaction,
            recovered.expiry_height,
        )?;
        Ok(())
    })
    .await?
    {
        SavedSubmission::Broadcast => Ok(AddressSubmission::Broadcast(
            store.mark_address_broadcast(&payment.id, &payment.txid)?,
        )),
        SavedSubmission::Expired => Ok(AddressSubmission::Expired(
            store.reset_address_for_retry(&payment.id, &payment.txid)?,
        )),
    }
}

enum SavedSubmission {
    Broadcast,
    Expired,
}

async fn submit_saved_prepared<
    R: PaymentSubmitter,
    F: FnOnce(&PreparedPayment) -> anyhow::Result<()>,
>(
    store: &Store,
    runtime: &R,
    id: &str,
    txid: &str,
    record_recovered: F,
) -> anyhow::Result<SavedSubmission> {
    if runtime.transaction_known(txid).await? {
        return Ok(SavedSubmission::Broadcast);
    }
    let prepared = match store.prepared_transaction(id) {
        Ok(prepared) => prepared,
        Err(missing) => {
            let recovered = runtime.recover_prepared(txid).await?.ok_or(missing)?;
            if recovered.txid != txid {
                anyhow::bail!("wallet returned a different prepared transaction");
            }
            record_recovered(&recovered)?;
            PreparedTransaction {
                raw_transaction: recovered.raw_transaction,
                expiry_height: recovered.expiry_height,
            }
        }
    };
    if prepared.expiry_height != 0 && runtime.chain_height().await? >= prepared.expiry_height {
        if runtime.transaction_known(txid).await? {
            return Ok(SavedSubmission::Broadcast);
        }
        return Ok(SavedSubmission::Expired);
    }
    if let Err(broadcast_error) = runtime.broadcast(&prepared.raw_transaction).await {
        match runtime.transaction_known(txid).await {
            Ok(true) => {}
            Ok(false) => return Err(broadcast_error),
            Err(lookup_error) => {
                return Err(lookup_error).with_context(|| {
                    format!(
                        "broadcast failed and transaction {txid} could not be checked: {broadcast_error}"
                    )
                });
            }
        }
    }
    Ok(SavedSubmission::Broadcast)
}

enum PreparedSubmission {
    Broadcast(Activity),
    Expired(Activity),
}

#[async_trait::async_trait]
trait FaucetRuntime: Sync {
    async fn prepare_payment(
        &self,
        activity_id: Option<&str>,
        seed: &str,
        destination: &str,
        amount_zatoshi: u64,
    ) -> anyhow::Result<PreparedPayment>;
    async fn chain_height(&self) -> anyhow::Result<u64>;
    async fn block_hash(&self, height: u32) -> anyhow::Result<String>;
    async fn synchronize_latest(&self) -> anyhow::Result<()>;
    async fn treasury_cursor(&self) -> anyhow::Result<Option<crate::db::TreasuryCursor>>;
    async fn initialize_treasury_cursor(
        &self,
        receiver: &str,
        height: u32,
        hash: &str,
    ) -> anyhow::Result<crate::db::TreasuryCursor>;
    async fn discover_reward(
        &self,
        cursor: &crate::db::TreasuryCursor,
        treasury: &Account,
    ) -> anyhow::Result<crate::db::TreasuryCursor>;
    async fn shield_coinbase(
        &self,
        seed: &str,
        treasury: &Account,
        minimum_net: u64,
    ) -> anyhow::Result<()>;
    async fn mine_and_sync(&self, blocks: u32) -> anyhow::Result<()>;
    async fn mine_to_height(&self, height: u64) -> anyhow::Result<()> {
        let current = self.chain_height().await?;
        if height > current {
            self.mine_and_sync(u32::try_from(height - current)?).await?;
        }
        anyhow::ensure!(
            self.chain_height().await? >= height,
            "maturity mining did not reach height {height}"
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl FaucetRuntime for AppState {
    async fn prepare_payment(
        &self,
        activity_id: Option<&str>,
        seed: &str,
        destination: &str,
        amount_zatoshi: u64,
    ) -> anyhow::Result<PreparedPayment> {
        self.0
            .wallet
            .prepare(
                activity_id,
                seed,
                TREASURY_ACCOUNT_ID,
                "ironwood",
                destination,
                amount_zatoshi,
                None,
            )
            .await
    }

    async fn chain_height(&self) -> anyhow::Result<u64> {
        Ok(self.0.rpc.chain_info().await?.blocks)
    }

    async fn block_hash(&self, height: u32) -> anyhow::Result<String> {
        self.0.rpc.block_hash(height).await
    }
    async fn synchronize_latest(&self) -> anyhow::Result<()> {
        AppState::synchronize_latest(self).await
    }
    async fn treasury_cursor(&self) -> anyhow::Result<Option<crate::db::TreasuryCursor>> {
        self.0.wallet.treasury_cursor().await
    }
    async fn initialize_treasury_cursor(
        &self,
        receiver: &str,
        height: u32,
        hash: &str,
    ) -> anyhow::Result<crate::db::TreasuryCursor> {
        self.0
            .wallet
            .initialize_treasury_cursor(receiver, height, hash)
            .await
    }
    async fn discover_reward(
        &self,
        cursor: &crate::db::TreasuryCursor,
        treasury: &Account,
    ) -> anyhow::Result<crate::db::TreasuryCursor> {
        let _sync = self.0.wallet_sync.lock().await;
        discover_reward(self, cursor, treasury).await
    }
    async fn shield_coinbase(
        &self,
        seed: &str,
        treasury: &Account,
        minimum_net: u64,
    ) -> anyhow::Result<()> {
        self.0
            .wallet
            .shield_coinbase(
                seed,
                TREASURY_ACCOUNT_ID,
                &treasury.transparent_address,
                &treasury.unified_address,
                minimum_net,
            )
            .await?;
        Ok(())
    }

    async fn mine_and_sync(&self, blocks: u32) -> anyhow::Result<()> {
        mine_and_sync(self, blocks).await?;
        Ok(())
    }
}

async fn discover_reward(
    state: &AppState,
    cursor: &crate::db::TreasuryCursor,
    treasury: &Account,
) -> anyhow::Result<crate::db::TreasuryCursor> {
    let next_height = cursor
        .height
        .checked_add(1)
        .context("treasury discovery height overflow")?;
    let block_hash = state.0.rpc.block_hash(next_height).await?;
    let block = state.0.rpc.block(&block_hash).await?;
    anyhow::ensure!(
        block.get("height").and_then(Value::as_u64) == Some(u64::from(next_height)),
        "Zakura returned the wrong treasury discovery height"
    );
    let candidate = coinbase_candidate(&block, &treasury.transparent_address)?;
    let mut required_spenders = Vec::new();
    if let Some(candidate) = &candidate {
        for index in &candidate.output_indexes {
            if state
                .0
                .rpc
                .unspent_output(&candidate.txid, *index)
                .await?
                .is_none()
            {
                let spenders = state
                    .0
                    .wallet
                    .known_spending_transactions(&candidate.txid, *index)
                    .await?;
                let mut accepted = Vec::new();
                for txid in spenders {
                    let Some(transaction) = state.0.rpc.lookup_transaction(&txid).await? else {
                        continue;
                    };
                    if let Some(hash) = confirmed_block_hash(&transaction) {
                        let height = state
                            .0
                            .rpc
                            .block(hash)
                            .await?
                            .get("height")
                            .and_then(Value::as_u64)
                            .context("spender block omitted its height")?;
                        anyhow::ensure!(
                            state.0.rpc.block_hash(u32::try_from(height)?).await? == hash,
                            "spender block is not canonical"
                        );
                    } else if !state.0.rpc.mempool().await?.contains(&txid) {
                        continue;
                    }
                    accepted.push((*index, txid));
                }
                anyhow::ensure!(
                    !accepted.is_empty(),
                    "treasury output has no known spender; discovery remains pending"
                );
                required_spenders.extend(accepted);
            }
        }
    }
    let checkpoint = ChainCheckpoint {
        height: u64::from(next_height),
        hash: block_hash,
    };
    anyhow::ensure!(
        state.0.rpc.block_hash(next_height).await? == checkpoint.hash,
        "canonical block changed during treasury discovery"
    );
    let next = state
        .0
        .wallet
        .commit_treasury_discovery(
            cursor,
            &checkpoint,
            candidate.as_ref().map(|value| value.raw.as_str()),
            &required_spenders,
        )
        .await?;
    anyhow::ensure!(
        state.0.rpc.block_hash(next_height).await? == checkpoint.hash,
        "canonical block changed after treasury discovery"
    );
    Ok(next)
}

async fn prepare_with_replenishment<R: FaucetRuntime>(
    runtime: &R,
    activity_id: Option<&str>,
    seed: &str,
    treasury: &Account,
    destination: &str,
    amount_zatoshi: u64,
) -> anyhow::Result<PreparedPayment> {
    let mut reconciled_after_scan_required = false;
    loop {
        match runtime
            .prepare_payment(activity_id, seed, destination, amount_zatoshi)
            .await
        {
            Ok(prepared) => return Ok(prepared),
            Err(error) => {
                if matches!(error.downcast_ref(), Some(PaymentError::ScanRequired)) {
                    if !reconciled_after_scan_required {
                        runtime.synchronize_latest().await?;
                        reconciled_after_scan_required = true;
                        continue;
                    }
                    if runtime.chain_height().await? < u64::from(WALLET_BIRTHDAY_HEIGHT) {
                        runtime
                            .mine_to_height(u64::from(WALLET_BIRTHDAY_HEIGHT))
                            .await?;
                        runtime.synchronize_latest().await?;
                        continue;
                    }
                    return Err(error);
                }
                let Some(PaymentError::InsufficientFunds {
                    available,
                    required,
                }) = error.downcast_ref()
                else {
                    return Err(error);
                };
                let minimum_net = required
                    .checked_sub(*available)
                    .filter(|value| *value > 0)
                    .context("SDK insufficient-funds result has no positive deficit")?;
                replenish_treasury(runtime, seed, treasury, minimum_net).await?;
                reconciled_after_scan_required = false;
            }
        }
    }
}

async fn replenish_treasury<R: FaucetRuntime>(
    runtime: &R,
    seed: &str,
    treasury: &Account,
    minimum_net: u64,
) -> anyhow::Result<()> {
    let boundary = WALLET_BIRTHDAY_HEIGHT - 1;
    runtime.mine_to_height(u64::from(boundary)).await?;
    let mut cursor = match runtime.treasury_cursor().await? {
        Some(cursor) => {
            anyhow::ensure!(
                cursor.receiver == treasury.transparent_address,
                "treasury cursor belongs to a different receiver"
            );
            cursor
        }
        None => {
            let hash = runtime.block_hash(boundary).await?;
            runtime
                .initialize_treasury_cursor(&treasury.transparent_address, boundary, &hash)
                .await?
        }
    };
    if runtime.chain_height().await? < u64::from(cursor.height)
        || runtime.block_hash(cursor.height).await? != cursor.block_hash
    {
        runtime.synchronize_latest().await?;
        cursor = runtime
            .treasury_cursor()
            .await?
            .context("treasury cursor disappeared during rewind")?;
        anyhow::ensure!(
            runtime.chain_height().await? >= u64::from(cursor.height)
                && runtime.block_hash(cursor.height).await? == cursor.block_hash,
            "treasury cursor is not canonical after wallet rewind"
        );
    }

    loop {
        match runtime.shield_coinbase(seed, treasury, minimum_net).await {
            Ok(()) => return runtime.mine_and_sync(1).await,
            Err(error) if matches!(error.downcast_ref(), Some(PaymentError::TreasuryExhausted)) => {
            }
            Err(error) => return Err(error),
        }
        let next_height = cursor
            .height
            .checked_add(1)
            .context("treasury discovery height overflow")?;
        // The SDK constructs a spend at tip + 1; 100 confirmations make this reward eligible there.
        let required_height = u64::from(next_height) + u64::from(COINBASE_MATURITY_BLOCKS) - 1;
        runtime.mine_to_height(required_height).await?;
        cursor = runtime.discover_reward(&cursor, treasury).await?;
    }
}

struct CoinbaseCandidate {
    txid: String,
    raw: String,
    output_indexes: Vec<u32>,
}

fn coinbase_candidate(block: &Value, receiver: &str) -> anyhow::Result<Option<CoinbaseCandidate>> {
    let height = block
        .get("height")
        .and_then(Value::as_u64)
        .and_then(|height| u32::try_from(height).ok())
        .context("Zakura block omitted its height")?;
    let transaction = block
        .get("tx")
        .and_then(Value::as_array)
        .and_then(|transactions| transactions.first())
        .context("Zakura block omitted its coinbase transaction")?;
    let raw = transaction
        .get("hex")
        .and_then(Value::as_str)
        .context("Zakura coinbase omitted its raw transaction")?;
    let bytes = hex::decode(raw).context("invalid treasury transaction hex")?;
    let decoded = zcash_primitives::transaction::Transaction::read(
        &bytes[..],
        zcash_protocol::consensus::BranchId::for_height(&regtest_network(), height.into()),
    )?;
    let txid = decoded.txid().to_string();
    anyhow::ensure!(
        transaction.get("txid").and_then(Value::as_str) == Some(txid.as_str()),
        "Zakura coinbase transaction ID does not match its raw transaction"
    );
    let bundle = decoded
        .transparent_bundle()
        .filter(|bundle| bundle.is_coinbase())
        .context("Zakura block's first transaction is not coinbase")?;
    let output_indexes = bundle
        .vout
        .iter()
        .enumerate()
        .filter_map(|(index, output)| {
            output
                .recipient_address()
                .filter(|address| address.encode(&regtest_network()) == receiver)
                .map(|_| u32::try_from(index).context("treasury output index exceeds u32"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if output_indexes.is_empty() {
        return Ok(None);
    }
    Ok(Some(CoinbaseCandidate {
        txid,
        raw: raw.to_owned(),
        output_indexes,
    }))
}

async fn mine_and_sync(state: &AppState, blocks: u32) -> anyhow::Result<Vec<String>> {
    let hashes = state.0.rpc.generate(blocks).await?;
    let tip_hash = hashes
        .last()
        .context("Zakura did not return the mined block hash")?;
    let tip_height = state
        .0
        .rpc
        .block(tip_hash)
        .await?
        .pointer("/height")
        .and_then(Value::as_u64)
        .context("Zakura mined block omitted its height")?;
    state.synchronize_wallet(Some(tip_height)).await?;
    // Every caller here produces blocks, so the chain moved for everyone, not
    // just the tab that asked. Without this, other dashboards keep the old
    // height and tip until something else happens to mine.
    notify(state, "chain");
    Ok(hashes)
}

pub async fn provision_initial_balance(state: &AppState) -> anyhow::Result<()> {
    const INITIAL_FUNDING_KEY: &str = "startup-account-1-ironwood-v1";
    let existing = state.0.store.activity_for_key(INITIAL_FUNDING_KEY)?;
    if existing
        .as_ref()
        .is_some_and(|activity| activity.status == "confirmed")
    {
        return Ok(());
    }
    if existing.is_none() {
        // A fresh wallet needs scanned blocks before a proposal can determine its target height.
        let seed = state.0.store.seed()?;
        let treasury = state.0.store.account(TREASURY_ACCOUNT_ID)?;
        replenish_treasury(state, &seed, &treasury, 1).await?;
    }
    fund_from_treasury(
        state,
        1,
        "ironwood",
        5 * ZATOSHIS_PER_ZEC,
        INITIAL_FUNDING_KEY,
    )
    .await?;
    let account = state
        .accounts()
        .await
        .into_iter()
        .find(|account| account.id == 1)
        .context("Account 1 disappeared during startup provisioning")?;
    anyhow::ensure!(
        account.ironwood_zatoshi == 5 * ZATOSHIS_PER_ZEC,
        "Account 1 startup Ironwood balance is {}, expected {} zatoshi",
        account.ironwood_zatoshi,
        5 * ZATOSHIS_PER_ZEC
    );
    Ok(())
}

struct ManualMiningAdapter(AppState);

#[async_trait::async_trait]
impl MiningRuntime for ManualMiningAdapter {
    async fn generate_one(&self) -> anyhow::Result<String> {
        let mut hashes = self.0.0.rpc.generate(1).await?;
        anyhow::ensure!(
            hashes.len() == 1 && !hashes[0].is_empty(),
            "Zakura must return exactly one nonempty mined block hash"
        );
        Ok(hashes.remove(0))
    }

    async fn synchronize(&self, tip_hash: &str) -> anyhow::Result<()> {
        let height = self
            .0
            .0
            .rpc
            .block(tip_hash)
            .await?
            .pointer("/height")
            .and_then(Value::as_u64)
            .context("Zakura mined block omitted its height")?;
        self.0.synchronize_wallet(Some(height)).await
    }

    fn notify(&self, topic: &'static str) {
        notify(&self.0, topic);
    }
}

impl AppState {
    async fn start_mining(&self, blocks: u32, key: String) -> ApiResult<Admission> {
        let runtime: Arc<dyn MiningRuntime> = Arc::new(ManualMiningAdapter(self.clone()));
        #[cfg(test)]
        let runtime = self.0.mining_runtime.clone().unwrap_or(runtime);
        Ok(self.0.mining.start(runtime, blocks, key).await?)
    }
}

#[derive(Deserialize)]
struct StartMiningRequest {
    blocks: u32,
    idempotency_key: String,
}

async fn start_mining_job(
    State(state): State<AppState>,
    Json(req): Json<StartMiningRequest>,
) -> ApiResult<(StatusCode, Json<MiningJob>)> {
    require_key(&req.idempotency_key)?;
    let admission = state.start_mining(req.blocks, req.idempotency_key).await?;
    Ok((StatusCode::ACCEPTED, Json(admission.job)))
}

async fn latest_mining_job(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"job":state.0.mining.latest().await}))
}

async fn mining_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<MiningJob>> {
    state
        .0
        .mining
        .get(&id)
        .await
        .map(Json)
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            message: "Mining job does not exist".into(),
        })
}

#[derive(Deserialize)]
struct MineRequest {
    blocks: u32,
}
async fn mine(
    State(state): State<AppState>,
    Json(req): Json<MineRequest>,
) -> ApiResult<Json<Value>> {
    if !(1..=10_000).contains(&req.blocks) {
        return Err(ApiError::bad_request("blocks must be between 1 and 10,000"));
    }
    let mut admission = state
        .start_mining(req.blocks, uuid::Uuid::new_v4().to_string())
        .await?;
    loop {
        let update = admission.updates.borrow_and_update().clone();
        match update.job.state {
            MiningState::Completed => {
                let hashes = update
                    .hashes
                    .context("Completed mining job omitted its hashes")?;
                return Ok(Json(json!({"blocks":hashes.len(),"hashes":*hashes})));
            }
            MiningState::Failed => {
                return Err(anyhow::anyhow!(
                    update
                        .job
                        .error
                        .context("Failed mining job omitted its error")?
                )
                .into());
            }
            MiningState::Mining | MiningState::Syncing => {}
        }
        admission
            .updates
            .changed()
            .await
            .context("Mining job update channel closed before completion")?;
    }
}

#[derive(Deserialize)]
struct SeedRequest {
    confirmation: String,
}
async fn seed(
    State(state): State<AppState>,
    Json(req): Json<SeedRequest>,
) -> ApiResult<Json<Value>> {
    if req.confirmation != "I understand this seed is for regtest only" {
        return Err(ApiError::bad_request("exact confirmation phrase required"));
    }
    Ok(Json(
        json!({"seed_hex":state.0.store.seed()?,"warning":"Never send real funds to this development seed."}),
    ))
}
async fn block(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
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
struct BlocksQuery {
    limit: Option<u32>,
    before: Option<u64>,
}
async fn blocks(
    State(state): State<AppState>,
    Query(query): Query<BlocksQuery>,
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

async fn transaction(
    State(state): State<AppState>,
    Path(txid): Path<String>,
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
async fn mempool(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"transactions":state.0.rpc.mempool().await?})))
}
async fn address(
    State(state): State<AppState>,
    Path(address): Path<String>,
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
struct SearchQuery {
    q: String,
}
async fn search(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> ApiResult<Json<Value>> {
    if query.q.starts_with('t') {
        return address(State(state), Path(query.q)).await;
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

const NO_BLOCK: &str = "No block at that height or hash on this chain.";
const NO_TRANSACTION: &str =
    "No transaction with that ID on this chain. It may not have been mined yet.";
const NO_BLOCK_OR_TRANSACTION: &str = "No block or transaction on this chain has that hash.";

/// Zakura answers an unknown or unparseable block or transaction with -5 or -8.
fn not_found(error: anyhow::Error, message: &str) -> ApiError {
    match error.downcast_ref::<RpcError>().map(|error| error.code) {
        Some(-5 | -8) => ApiError {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        },
        _ => error.into(),
    }
}

async fn events(
    State(state): State<AppState>,
) -> Sse<impl futures_core::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let mut receiver = state.0.events.subscribe();
    let stream = async_stream::stream! { loop { match receiver.recv().await { Ok(data) => yield Ok(Event::default().event("update").data(data)), Err(broadcast::error::RecvError::Lagged(_)) => continue, Err(_) => break } } };
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn confirmed_block_hash(tx: &Value) -> Option<&str> {
    let confirmations = tx.get("confirmations").and_then(Value::as_u64).unwrap_or(0);
    if confirmations == 0 {
        return None;
    }
    tx.get("blockhash")
        .and_then(Value::as_str)
        .filter(|hash| !hash.is_empty())
}

fn apply_confirmation(store: &Store, pending: &Activity, tx: &Value) -> anyhow::Result<Activity> {
    match confirmed_block_hash(tx) {
        Some(hash) => store.confirm(&pending.id, &pending.txid, hash),
        None => Ok(pending.clone()),
    }
}

async fn confirm_from_chain(state: &AppState, pending: Activity) -> anyhow::Result<Activity> {
    if pending.status == "confirmed" {
        return Ok(pending);
    }
    match state.0.rpc.transaction(&pending.txid).await {
        Ok(tx) => {
            let updated = apply_confirmation(&state.0.store, &pending, &tx)?;
            if updated.status == "confirmed" {
                notify(state, "wallet");
            }
            Ok(updated)
        }
        Err(error) => {
            tracing::warn!(
                error = %format_args!("{error:#}"),
                txid = %pending.txid,
                "could not fetch transaction for confirmation"
            );
            Ok(pending)
        }
    }
}

async fn reconcile_unconfirmed(state: &AppState) -> anyhow::Result<()> {
    for activity in state.0.store.unconfirmed_activities()? {
        confirm_from_chain(state, activity).await?;
    }
    Ok(())
}

async fn confirm_after_mining(state: &AppState, pending: Activity) -> anyhow::Result<Activity> {
    let pending = confirm_from_chain(state, pending).await?;
    if pending.status == "confirmed" {
        return Ok(pending);
    }
    match mine_and_sync(state, 1).await {
        Ok(_) => confirm_from_chain(state, pending).await,
        Err(error) => {
            tracing::warn!(
                error = %format_args!("{error:#}"),
                activity = %pending.id,
                "transaction recorded but auto-mine failed"
            );
            Ok(pending)
        }
    }
}
fn notify(state: &AppState, topic: &str) {
    let _ = state.0.events.send(topic.to_owned());
}
fn require_key(key: &str) -> ApiResult<()> {
    validate_idempotency_key(key).map_err(ApiError::bad_request)
}

fn require_pool(pool: &str, field: &str) -> ApiResult<()> {
    match pool {
        "transparent" | "ironwood" => Ok(()),
        // NU6.3 is active from block 1, so the Orchard pool can neither receive
        // nor hold funds on this chain.
        "orchard" => Err(ApiError::bad_request(format!(
            "{field} orchard is unavailable: NU6.3 is active and Orchard no longer accepts deposits; use ironwood"
        ))),
        _ => Err(ApiError::bad_request(format!(
            "{field} must be transparent or ironwood"
        ))),
    }
}

/// Checks the whole send request without touching the store, wallet or node,
/// and returns the encoded memo it carries.
fn validate_send(req: &SendRequest) -> ApiResult<Option<MemoBytes>> {
    require_key(&req.idempotency_key)?;
    require_user_account(req.from_account)?;
    require_user_account(req.to_account)?;
    require_pool(&req.source_pool, "source_pool")?;
    require_pool(&req.destination_pool, "destination_pool")?;
    if req.from_account == req.to_account && req.source_pool == req.destination_pool {
        return Err(ApiError::bad_request(
            "choose a different account or a different destination pool",
        ));
    }
    if req.amount_zatoshi == 0 || req.amount_zatoshi > MAX_MONEY {
        return Err(ApiError::bad_request(format!(
            "amount_zatoshi must be between 1 and {MAX_MONEY}"
        )));
    }
    parse_memo(req.memo.as_deref(), &req.destination_pool)
}

/// An absent (or null) memo is `None`. Any present memo, including `""`, is
/// encoded as an explicit ZIP-302 text memo and is only valid for ironwood.
fn parse_memo(memo: Option<&str>, destination_pool: &str) -> ApiResult<Option<MemoBytes>> {
    let Some(text) = memo else {
        return Ok(None);
    };
    if destination_pool != "ironwood" {
        return Err(anyhow::Error::new(PaymentError::TransparentMemo).into());
    }
    // Text memos are zero-padded to 512 bytes, so a trailing NUL could not be
    // told apart from padding and would be silently lost on decode.
    if text.ends_with('\0') {
        return Err(ApiError::bad_request(
            "memo must not end with a NUL (U+0000) character",
        ));
    }
    MemoBytes::from_bytes(text.as_bytes())
        .map(Some)
        .map_err(|error| ApiError::bad_request(format!("invalid memo: {error}")))
}

fn require_user_account(id: u8) -> ApiResult<()> {
    if (1..=USER_ACCOUNT_COUNT).contains(&id) {
        Ok(())
    } else {
        Err(ApiError::bad_request("account must be between 1 and 5"))
    }
}

fn require_faucet_address(value: &str) -> ApiResult<()> {
    match Address::decode(&regtest_network(), value) {
        Some(Address::Unified(_) | Address::Transparent(_)) => Ok(()),
        Some(_) => Err(ApiError::bad_request(
            "destination must be a unified or transparent Regtest address",
        )),
        None => Err(ApiError::bad_request(
            "destination is not a valid Regtest address",
        )),
    }
}

fn require_transparent_address(value: &str) -> ApiResult<()> {
    match Address::decode(&regtest_network(), value) {
        Some(Address::Transparent(_)) => Ok(()),
        _ => Err(ApiError::bad_request(
            "only transparent addresses have public explorer activity",
        )),
    }
}

type ApiResult<T> = Result<T, ApiError>;
struct ApiError {
    status: StatusCode,
    message: String,
}
impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
}
impl From<MiningAdmissionError> for ApiError {
    fn from(error: MiningAdmissionError) -> Self {
        let status = match error {
            MiningAdmissionError::InvalidBlocks | MiningAdmissionError::InvalidKey => {
                StatusCode::BAD_REQUEST
            }
            MiningAdmissionError::KeyConflict | MiningAdmissionError::Busy => StatusCode::CONFLICT,
            MiningAdmissionError::Capacity => StatusCode::SERVICE_UNAVAILABLE,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        let status = if error.downcast_ref::<IdempotencyConflict>().is_some() {
            StatusCode::CONFLICT
        } else {
            match error.downcast_ref::<PaymentError>() {
                Some(PaymentError::TreasuryExhausted) => StatusCode::SERVICE_UNAVAILABLE,
                Some(PaymentError::InsufficientFunds { .. }) => StatusCode::UNPROCESSABLE_ENTITY,
                Some(PaymentError::TransparentMemo) => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            }
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %format_args!("{error:#}"), "request failed");
        }
        Self {
            status,
            message: error.to_string(),
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error":{"message":self.message,"status":self.status.as_u16()}})),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        },
    };

    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[derive(Clone, Default)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs<T>(operation: impl FnOnce() -> T) -> (T, String) {
        let output = LogWriter::default();
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        // Keep capture scoped to the synchronous operation so parallel tests and
        // tasks running on other threads cannot write into this test's log.
        let result = tracing::subscriber::with_default(subscriber, operation);
        let logs = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        (result, logs)
    }

    /// A dashboard directory containing a recognisable shell and one asset.
    fn dashboard() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("index.html"),
            "<!doctype html><div id=\"root\">",
        )
        .expect("write shell");
        std::fs::create_dir(dir.path().join("assets")).expect("assets dir");
        std::fs::write(dir.path().join("assets/app.js"), "console.log(1)").expect("write asset");
        dir
    }

    async fn get(dir: &tempfile::TempDir, path: &str) -> (StatusCode, String) {
        let response = dashboard_router(dir.path().to_path_buf())
            .oneshot(Request::get(path).body(Body::empty()).expect("request"))
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The dashboard is a single-page app: a path it owns is a route, not a
    /// missing file. This previously returned 404 for every path but `/`,
    /// because `not_found_service` wraps the fallback in `SetStatus(404)`.
    #[tokio::test]
    async fn client_side_routes_return_the_shell() {
        let dir = dashboard();
        for path in [
            "/",
            "/wallet",
            "/explorer",
            "/explorer/block/1209",
            "/network",
        ] {
            let (status, body) = get(&dir, path).await;
            assert_eq!(status, StatusCode::OK, "{path} should serve the shell");
            assert!(
                body.contains("id=\"root\""),
                "{path} should return the shell markup"
            );
        }
    }

    /// The fallback is scoped, not a catch-all. A missing asset answered with
    /// HTML would be parsed by the browser as JavaScript or CSS.
    #[tokio::test]
    async fn missing_assets_and_unknown_api_paths_stay_404() {
        let dir = dashboard();
        for path in [
            "/api/v1/nope",
            "/assets/does-not-exist.js",
            "/favicon.ico",
            "/nested/path/styles.css",
        ] {
            let (status, body) = get(&dir, path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path} should be a 404");
            assert!(
                !body.contains("id=\"root\""),
                "{path} must not return the shell"
            );
        }
    }

    #[tokio::test]
    async fn real_assets_are_still_served() {
        let dir = dashboard();
        let (status, body) = get(&dir, "/assets/app.js").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "console.log(1)");
    }

    /// A misconfigured THS_WEB_DIR should fail visibly rather than serving an
    /// empty 200 that looks like a working dashboard.
    #[tokio::test]
    async fn a_missing_shell_is_reported_rather_than_served_empty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (status, _) = get(&dir, "/wallet").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn address_faucet_resolves_only_published_user_addresses() {
        let (state, _dir) = state_with_local_wallet();
        for id in 1..=USER_ACCOUNT_COUNT {
            let account = state.0.store.account(id).unwrap();
            assert_eq!(
                internal_faucet_destination(&state.0.store, &account.transparent_address).unwrap(),
                Some((id, "transparent"))
            );
            assert_eq!(
                internal_faucet_destination(&state.0.store, &account.unified_address).unwrap(),
                Some((id, "ironwood"))
            );
            assert_eq!(
                internal_faucet_destination(
                    &state.0.store,
                    &format!("{} ", account.transparent_address)
                )
                .unwrap(),
                None
            );
        }
        let treasury = state.0.store.account(TREASURY_ACCOUNT_ID).unwrap();
        for address in [
            treasury.transparent_address,
            treasury.unified_address,
            "unmatched".into(),
        ] {
            assert_eq!(
                internal_faucet_destination(&state.0.store, &address).unwrap(),
                None
            );
        }
    }

    #[test]
    fn address_faucet_resolver_propagates_store_errors() {
        let store = Store::open(":memory:").unwrap();
        assert!(internal_faucet_destination(&store, "unmatched").is_err());
    }

    fn address_faucet_activity(store: &Store) -> Activity {
        let row = store
            .claim_faucet(2, "transparent", 100_000_000, "adapter-test")
            .unwrap();
        let row = store
            .record_prepared(&row.id, "faucet-txid", b"prepared bytes", 140)
            .unwrap();
        store.mark_broadcast(&row.id, &row.txid).unwrap()
    }

    #[test]
    fn address_faucet_response_preserves_pending_and_confirmed_status() {
        let (state, _dir) = state_with_local_wallet();
        let address = state.0.store.account(2).unwrap().transparent_address;
        let request = || FaucetAddressRequest {
            idempotency_key: "address-test-key".into(),
            address: address.clone(),
            amount_zatoshi: 100_000_000,
        };
        let row = address_faucet_activity(&state.0.store);
        let pending = faucet_address_response(request(), row.clone()).unwrap();
        assert_eq!(pending.status, "broadcast");
        assert_eq!(pending.block_hash, None);
        let row = state
            .0
            .store
            .confirm(&row.id, &row.txid, "inclusion-hash")
            .unwrap();
        let value =
            serde_json::to_value(faucet_address_response(request(), row.clone()).unwrap()).unwrap();
        assert_eq!(
            value,
            json!({"address": address, "amount_zatoshi": 100_000_000, "txid": "faucet-txid", "block_hash": "inclusion-hash", "status": "confirmed"})
        );
        for block_hash in [None, Some(String::new())] {
            let mut invalid = row.clone();
            invalid.block_hash = block_hash;
            assert!(faucet_address_response(request(), invalid).is_err());
        }
    }

    struct RecordingAddressFaucetRuntime {
        internal_calls: std::sync::Mutex<Vec<(u8, String, u64, String)>>,
        external_calls: std::sync::Mutex<Vec<(String, u64)>>,
        activity: Activity,
        fail_internal: bool,
        fail_external: bool,
    }

    impl RecordingAddressFaucetRuntime {
        fn new(store: &Store) -> Self {
            let row = address_faucet_activity(store);
            Self {
                internal_calls: Default::default(),
                external_calls: Default::default(),
                activity: store.confirm(&row.id, &row.txid, "inclusion-hash").unwrap(),
                fail_internal: false,
                fail_external: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl AddressFaucetRuntime for RecordingAddressFaucetRuntime {
        async fn fund_internal(
            &self,
            id: u8,
            pool: &str,
            amount: u64,
            key: &str,
        ) -> anyhow::Result<Activity> {
            self.internal_calls
                .lock()
                .unwrap()
                .push((id, pool.into(), amount, key.into()));
            anyhow::ensure!(!self.fail_internal, "internal funding failed");
            Ok(self.activity.clone())
        }

        async fn fund_external(
            &self,
            req: FaucetAddressRequest,
        ) -> anyhow::Result<FaucetAddressResponse> {
            self.external_calls
                .lock()
                .unwrap()
                .push((req.address.clone(), req.amount_zatoshi));
            anyhow::ensure!(!self.fail_external, "external funding failed");
            Ok(FaucetAddressResponse {
                address: req.address,
                amount_zatoshi: req.amount_zatoshi,
                txid: "external-txid".into(),
                block_hash: Some("external-block".into()),
                status: "confirmed".into(),
            })
        }
    }

    #[tokio::test]
    async fn address_faucet_dispatches_internal_transparent_and_unified() {
        let (state, _dir) = state_with_local_wallet();
        let runtime = RecordingAddressFaucetRuntime::new(&state.0.store);
        let account = state.0.store.account(2).unwrap();
        for (address, pool) in [
            (account.transparent_address, "transparent"),
            (account.unified_address, "ironwood"),
        ] {
            let Json(response) = execute_address_faucet(
                &state.0.store,
                &runtime,
                FaucetAddressRequest {
                    idempotency_key: "address-test-key".into(),
                    address: address.clone(),
                    amount_zatoshi: 100_000_000,
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
            assert_eq!(
                serde_json::to_value(response).unwrap(),
                json!({"address": address, "amount_zatoshi": 100_000_000, "txid": "faucet-txid", "block_hash": "inclusion-hash", "status": "confirmed"})
            );
            let calls = runtime.internal_calls.lock().unwrap();
            let call = calls.last().unwrap();
            assert_eq!((call.0, call.1.as_str(), call.2), (2, pool, 100_000_000));
        }
        assert!(runtime.external_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn address_faucet_retries_preserve_the_client_key() {
        let (state, _dir) = state_with_local_wallet();
        let runtime = RecordingAddressFaucetRuntime::new(&state.0.store);
        let address = state.0.store.account(2).unwrap().transparent_address;
        for _ in 0..2 {
            let Json(_response) = execute_address_faucet(
                &state.0.store,
                &runtime,
                FaucetAddressRequest {
                    idempotency_key: "address-test-key".into(),
                    address: address.clone(),
                    amount_zatoshi: 100_000_000,
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        }
        let calls = runtime.internal_calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].3, "address-test-key");
        assert_eq!(calls[1].3, "address-test-key");
        assert!(calls.iter().all(|call| require_key(&call.3).is_ok()));
        assert!(runtime.external_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn address_faucet_validation_precedes_dispatch() {
        let (state, _dir) = state_with_local_wallet();
        let runtime = RecordingAddressFaucetRuntime::new(&state.0.store);
        let address = state.0.store.account(2).unwrap().transparent_address;
        for (address, amount) in [
            ("not-an-address".into(), 1),
            (address.clone(), 0),
            (address.clone(), 500_000_001),
        ] {
            let error = execute_address_faucet(
                &state.0.store,
                &runtime,
                FaucetAddressRequest {
                    idempotency_key: "address-test-key".into(),
                    address,
                    amount_zatoshi: amount,
                },
            )
            .await
            .err()
            .unwrap();
            assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
        }
        assert!(runtime.internal_calls.lock().unwrap().is_empty());
        assert!(runtime.external_calls.lock().unwrap().is_empty());
        let Json(_response) = execute_address_faucet(
            &state.0.store,
            &runtime,
            FaucetAddressRequest {
                idempotency_key: "address-test-key".into(),
                address,
                amount_zatoshi: 500_000_000,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(runtime.internal_calls.lock().unwrap()[0].2, 500_000_000);
    }

    #[tokio::test]
    async fn address_faucet_dispatch_preserves_external_behavior() {
        let (state, _dir) = state_with_local_wallet();
        let mut runtime = RecordingAddressFaucetRuntime::new(&state.0.store);
        let treasury = state.0.store.account(TREASURY_ACCOUNT_ID).unwrap();
        for address in [
            treasury.transparent_address.clone(),
            treasury.unified_address,
        ] {
            let Json(response) = execute_address_faucet(
                &state.0.store,
                &runtime,
                FaucetAddressRequest {
                    idempotency_key: "address-test-key".into(),
                    address: address.clone(),
                    amount_zatoshi: 100_000_000,
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
            assert_eq!(
                serde_json::to_value(response).unwrap(),
                json!({"address": address, "amount_zatoshi": 100_000_000, "txid": "external-txid", "block_hash": "external-block", "status": "confirmed"})
            );
            assert_eq!(
                runtime.external_calls.lock().unwrap().last().unwrap(),
                &(address, 100_000_000)
            );
        }
        runtime.fail_external = true;
        assert!(
            execute_address_faucet(
                &state.0.store,
                &runtime,
                FaucetAddressRequest {
                    idempotency_key: "address-test-key".into(),
                    address: treasury.transparent_address,
                    amount_zatoshi: 1
                }
            )
            .await
            .is_err()
        );
        assert!(runtime.internal_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn address_faucet_internal_errors_do_not_fall_back() {
        let (state, _dir) = state_with_local_wallet();
        let mut runtime = RecordingAddressFaucetRuntime::new(&state.0.store);
        let address = state.0.store.account(2).unwrap().transparent_address;
        runtime.fail_internal = true;
        assert!(
            execute_address_faucet(
                &state.0.store,
                &runtime,
                FaucetAddressRequest {
                    idempotency_key: "address-test-key".into(),
                    address: address.clone(),
                    amount_zatoshi: 100_000_000
                }
            )
            .await
            .is_err()
        );
        runtime.fail_internal = false;
        runtime.activity.status = "broadcast".into();
        runtime.activity.block_hash = None;
        let Json(pending) = execute_address_faucet(
            &state.0.store,
            &runtime,
            FaucetAddressRequest {
                idempotency_key: "address-test-key".into(),
                address: address.clone(),
                amount_zatoshi: 100_000_000,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(pending.status, "broadcast");
        assert_eq!(pending.block_hash, None);
        assert_eq!(runtime.internal_calls.lock().unwrap().len(), 2);
        let broken_store = Store::open(":memory:").unwrap();
        assert!(
            execute_address_faucet(
                &broken_store,
                &runtime,
                FaucetAddressRequest {
                    idempotency_key: "address-test-key".into(),
                    address,
                    amount_zatoshi: 1
                }
            )
            .await
            .is_err()
        );
        assert_eq!(runtime.internal_calls.lock().unwrap().len(), 2);
        assert!(runtime.external_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn mining_adapter_rejects_missing_empty_and_multiple_rpc_hashes() {
        for hashes in [json!([]), json!([""]), json!(["hash-one", "hash-two"])] {
            let calls = Arc::new(AtomicUsize::new(0));
            let rpc_calls = calls.clone();
            let rpc = Router::new().route("/", post(move |Json(request): Json<Value>| {
                let hashes = hashes.clone();
                let calls = rpc_calls.clone();
                async move {
                    assert_eq!(request["method"], "generate");
                    assert_eq!(request["params"], json!([1]));
                    calls.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"jsonrpc":"2.0","id":request["id"],"result":hashes,"error":null}))
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, rpc).await.unwrap();
            });
            let (mut state, _dir) = state_with_local_wallet();
            Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(endpoint);
            let app = router(state);
            let (status, _) = mining_request(
                &app,
                "POST",
                "/api/v1/mining/jobs",
                json!({"blocks":2,"idempotency_key":"malformed-hashes"}),
            )
            .await;
            assert_eq!(status, StatusCode::ACCEPTED);
            let failed = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let (_, value) =
                        mining_request(&app, "GET", "/api/v1/mining/jobs", Value::Null).await;
                    if value["job"]["state"] == "failed" {
                        break value["job"].clone();
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(failed["completed_blocks"], 0);
            assert_eq!(failed["progress_uncertain"], true);
            assert!(
                failed["error"]
                    .as_str()
                    .unwrap()
                    .contains("exactly one nonempty")
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                mining_request(
                    &app,
                    "POST",
                    "/api/v1/mining/jobs",
                    json!({"blocks":2,"idempotency_key":"malformed-hashes"})
                )
                .await,
                (StatusCode::ACCEPTED, failed)
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
        }
    }

    #[tokio::test]
    async fn mining_adapter_accepts_one_nonempty_rpc_hash_and_forwards_events() {
        let rpc = Router::new().route(
            "/",
            post(|Json(request): Json<Value>| async move {
                assert_eq!(request["method"], "generate");
                assert_eq!(request["params"], json!([1]));
                Json(json!({"result":["one-hash"],"error":null,"id":request["id"]}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, rpc).await.unwrap();
        });
        let (mut state, _dir) = state_with_local_wallet();
        Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(endpoint);
        let mut events = state.0.events.subscribe();
        let adapter = ManualMiningAdapter(state);
        assert_eq!(adapter.generate_one().await.unwrap(), "one-hash");
        adapter.notify("mining");
        assert_eq!(events.recv().await.unwrap(), "mining");
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    struct ManualMiningRuntime {
        permits: tokio::sync::Semaphore,
        sync_permits: tokio::sync::Semaphore,
        calls: AtomicUsize,
        entered: tokio::sync::Notify,
        fail_sync: bool,
    }

    impl ManualMiningRuntime {
        fn blocked() -> Self {
            Self {
                permits: tokio::sync::Semaphore::new(0),
                sync_permits: tokio::sync::Semaphore::new(1),
                calls: AtomicUsize::new(0),
                entered: tokio::sync::Notify::new(),
                fail_sync: false,
            }
        }

        async fn wait_for_first_call(&self) {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let notified = self.entered.notified();
                    if self.calls.load(Ordering::SeqCst) > 0 {
                        return;
                    }
                    notified.await;
                }
            })
            .await
            .unwrap();
        }

        fn release(&self, blocks: usize) {
            self.permits.add_permits(blocks);
        }
    }

    #[async_trait::async_trait]
    impl MiningRuntime for ManualMiningRuntime {
        async fn generate_one(&self) -> anyhow::Result<String> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.entered.notify_one();
            self.permits.acquire().await.unwrap().forget();
            Ok(format!("hash-{call}"))
        }

        async fn synchronize(&self, tip_hash: &str) -> anyhow::Result<()> {
            assert!(tip_hash.starts_with("hash-"));
            self.sync_permits.acquire().await.unwrap().forget();
            anyhow::ensure!(!self.fail_sync, "injected wallet sync failure");
            Ok(())
        }

        fn notify(&self, _topic: &'static str) {}
    }

    fn state_with_mining(
        runtime: Arc<dyn MiningRuntime>,
        capacity: usize,
    ) -> (AppState, tempfile::TempDir) {
        let (mut state, dir) = state_with_local_wallet();
        let inner = Arc::get_mut(&mut state.0).unwrap();
        inner.mining_runtime = Some(runtime);
        inner.mining = Arc::new(MiningCoordinator::with_capacity(capacity));
        (state, dir)
    }

    async fn mining_request(
        app: &Router,
        method: &str,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn completed_mining(app: &Router) -> Value {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let (status, value) =
                    mining_request(app, "GET", "/api/v1/mining/jobs", Value::Null).await;
                assert_eq!(status, StatusCode::OK);
                if value["job"]["state"] == "completed" {
                    return value["job"].clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn mining_routes_validate_admit_replay_and_preserve_capacity_history() {
        let runtime = Arc::new(ManualMiningRuntime::blocked());
        let (state, _dir) = state_with_mining(runtime.clone(), 1);
        let app = router(state);
        assert_eq!(
            mining_request(&app, "GET", "/api/v1/mining/jobs", Value::Null).await,
            (StatusCode::OK, json!({"job":null}))
        );
        for body in [
            json!({"blocks":0,"idempotency_key":"valid-key"}),
            json!({"blocks":10001,"idempotency_key":"valid-key"}),
            json!({"blocks":1,"idempotency_key":"short"}),
            json!({"blocks":1,"idempotency_key":"space key"}),
            json!({"blocks":1,"idempotency_key":"nonasciié"}),
            json!({"blocks":1,"idempotency_key":"x".repeat(129)}),
            json!({"blocks":1,"idempotency_key":"control\n"}),
        ] {
            let (status, value) = mining_request(&app, "POST", "/api/v1/mining/jobs", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(value["error"]["status"], 400);
            assert!(value["error"]["message"].as_str().unwrap().len() > 5);
        }
        let body = json!({"blocks":3,"idempotency_key":"valid-key"});
        let (status, first) =
            mining_request(&app, "POST", "/api/v1/mining/jobs", body.clone()).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(first["requested_blocks"], 3);
        assert_eq!(first["completed_blocks"], 0);
        assert_eq!(first["state"], "mining");
        assert_eq!(first["error"], Value::Null);
        assert_eq!(first["progress_uncertain"], false);
        let id = first["id"].as_str().unwrap();
        assert_eq!(
            mining_request(
                &app,
                "GET",
                &format!("/api/v1/mining/jobs/{id}"),
                Value::Null
            )
            .await,
            (StatusCode::OK, first.clone())
        );
        assert_eq!(
            mining_request(&app, "POST", "/api/v1/mining/jobs", body.clone()).await,
            (StatusCode::ACCEPTED, first.clone())
        );
        for conflict in [
            json!({"blocks":2,"idempotency_key":"valid-key"}),
            json!({"blocks":3,"idempotency_key":"another-key"}),
        ] {
            let (status, error) =
                mining_request(&app, "POST", "/api/v1/mining/jobs", conflict).await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(error["error"]["status"], 409);
        }
        let (status, error) =
            mining_request(&app, "POST", "/api/v1/mine", json!({"blocks":1})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error["error"]["status"], 409);
        let (status, error) =
            mining_request(&app, "GET", "/api/v1/mining/jobs/unknown", Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error["error"]["status"], 404);
        runtime.release(3);
        let completed = completed_mining(&app).await;
        assert_eq!(completed["completed_blocks"], 3);
        assert_eq!(
            mining_request(&app, "POST", "/api/v1/mining/jobs", body).await,
            (StatusCode::ACCEPTED, completed.clone())
        );
        assert_eq!(
            mining_request(
                &app,
                "GET",
                &format!("/api/v1/mining/jobs/{id}"),
                Value::Null
            )
            .await,
            (StatusCode::OK, completed)
        );
        let (status, error) = mining_request(
            &app,
            "POST",
            "/api/v1/mining/jobs",
            json!({"blocks":1,"idempotency_key":"another-key"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error["error"]["status"], 503);
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("capacity")
        );
        let (status, error) =
            mining_request(&app, "POST", "/api/v1/mine", json!({"blocks":1})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error["error"]["status"], 503);
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn mining_legacy_waits_for_sync_and_preserves_full_response() {
        let runtime = Arc::new(ManualMiningRuntime {
            sync_permits: tokio::sync::Semaphore::new(0),
            ..ManualMiningRuntime::blocked()
        });
        let (state, _dir) = state_with_mining(runtime.clone(), 2);
        let app = router(state);
        for blocks in [0, 10001] {
            assert_eq!(
                mining_request(&app, "POST", "/api/v1/mine", json!({"blocks":blocks}))
                    .await
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        let request_app = app.clone();
        let task = tokio::spawn(async move {
            mining_request(&request_app, "POST", "/api/v1/mine", json!({"blocks":3})).await
        });
        runtime.wait_for_first_call().await;
        assert!(!task.is_finished());
        runtime.release(3);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let (_, value) =
                    mining_request(&app, "GET", "/api/v1/mining/jobs", Value::Null).await;
                if value["job"]["state"] == "syncing" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!task.is_finished());
        runtime.sync_permits.add_permits(1);
        assert_eq!(
            task.await.unwrap(),
            (
                StatusCode::OK,
                json!({"blocks":3,"hashes":["hash-1","hash-2","hash-3"]})
            )
        );
    }

    #[tokio::test]
    async fn mining_legacy_sync_failure_uses_error_envelope() {
        let runtime = Arc::new(ManualMiningRuntime {
            fail_sync: true,
            ..ManualMiningRuntime::blocked()
        });
        runtime.release(1);
        let (state, _dir) = state_with_mining(runtime, 1);
        let (status, error) =
            mining_request(&router(state), "POST", "/api/v1/mine", json!({"blocks":1})).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error["error"]["status"], 500);
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("Blocks mined, but wallet synchronization failed")
        );
    }

    #[tokio::test]
    async fn mining_survives_http_disconnect_and_discarded_async_response() {
        let runtime = Arc::new(ManualMiningRuntime::blocked());
        let (state, _dir) = state_with_mining(runtime.clone(), 2);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        let url = base_url.clone();
        let client_task = tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{url}/api/v1/mine"))
                .json(&json!({"blocks":3}))
                .send()
                .await
        });
        runtime.wait_for_first_call().await;
        client_task.abort();
        assert!(client_task.await.unwrap_err().is_cancelled());
        runtime.release(3);
        let client = reqwest::Client::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let value: Value = client
                    .get(format!("{base_url}/api/v1/mining/jobs"))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if value["job"]["state"] == "completed" {
                    assert_eq!(value["job"]["completed_blocks"], 3);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let body = json!({"blocks":1,"idempotency_key":"discarded-response"});
        drop(
            client
                .post(format!("{base_url}/api/v1/mining/jobs"))
                .json(&body)
                .send()
                .await
                .unwrap(),
        );
        let replay = client
            .post(format!("{base_url}/api/v1/mining/jobs"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::ACCEPTED);
        let replay: Value = replay.json().await.unwrap();
        runtime.sync_permits.add_permits(1);
        runtime.release(1);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let value: Value = client
                    .get(format!(
                        "{base_url}/api/v1/mining/jobs/{}",
                        replay["id"].as_str().unwrap()
                    ))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if value["state"] == "completed" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 4);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    fn state_with_local_wallet() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(dir.path().join("server.db")).expect("open store");
        store.initialize().expect("initialize store");
        let wallet =
            RealWallet::open(dir.path(), &store.seed().expect("wallet seed")).expect("open wallet");
        (
            AppState::new(store, wallet, "http://127.0.0.1:1".into(), "test".into()),
            dir,
        )
    }

    #[tokio::test]
    async fn status_counts_accounts_without_reading_the_seed() {
        let (state, dir) = state_with_local_wallet();
        rusqlite::Connection::open(dir.path().join("server.db"))
            .unwrap()
            .execute("UPDATE metadata SET value='not-hex' WHERE key='seed'", [])
            .unwrap();

        let response = router(state)
            .oneshot(Request::get("/api/v1/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let status: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status["account_count"], 5);
    }

    #[tokio::test]
    async fn account_reads_return_the_snapshot_without_contacting_lightwalletd() {
        let (state, _dir) = state_with_local_wallet();
        {
            let mut snapshot = state.0.wallet_snapshot.write().await;
            snapshot.accounts[0].ironwood_zatoshi = 400_000_000;
        }
        let response = router(state)
            .oneshot(
                Request::get("/api/v1/accounts")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        let accounts = value.as_array().expect("account array");
        assert_eq!(
            accounts
                .iter()
                .map(|a| a["id"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [1, 2, 3, 4, 5],
        );
        assert_eq!(accounts[0]["ironwood_zatoshi"], 400_000_000);
        let expected_keys = std::collections::BTreeSet::from([
            "id",
            "name",
            "unified_address",
            "transparent_address",
            "transparent_zatoshi",
            "ironwood_zatoshi",
            "unified_full_viewing_key",
        ]);
        for account in accounts {
            let keys = account
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(keys, expected_keys);
            assert!(
                account["unified_full_viewing_key"]
                    .as_str()
                    .is_some_and(|key| !key.is_empty())
            );
        }
    }

    #[tokio::test]
    async fn a_scan_behind_the_tip_publishes_only_while_it_is_canonical() {
        let checkpoint = |height, hash: &str| ChainCheckpoint {
            height,
            hash: hash.into(),
        };
        let (target, tip) = (checkpoint(10, "a"), checkpoint(12, "c"));
        let publishable = |scanned: Option<ChainCheckpoint>, canonical: &'static str| {
            let (target, tip) = (target.clone(), tip.clone());
            async move {
                scan_is_publishable(scanned.as_ref(), &target, &tip, |_| async move {
                    Ok(canonical.to_owned())
                })
                .await
                .unwrap()
            }
        };
        assert!(publishable(Some(checkpoint(12, "c")), "c").await);
        // mined past after reaching the target.
        assert!(publishable(Some(checkpoint(10, "a")), "a").await);
        assert!(publishable(Some(checkpoint(11, "b")), "b").await);
        // reorganized.
        assert!(!publishable(Some(checkpoint(10, "a")), "x").await);
        // short of the target.
        assert!(!publishable(Some(checkpoint(9, "z")), "z").await);
        assert!(!publishable(Some(checkpoint(12, "z")), "c").await);
        assert!(!publishable(Some(checkpoint(13, "d")), "c").await);
        assert!(!publishable(None, "c").await);
        // the tip dropped below the target.
        assert!(
            !scan_is_publishable(
                Some(&checkpoint(10, "a")),
                &checkpoint(12, "c"),
                &checkpoint(10, "a"),
                |_| async { unreachable!() },
            )
            .await
            .unwrap()
        );
        let below_birthday = checkpoint(u64::from(WALLET_BIRTHDAY_HEIGHT) - 1, "a");
        assert!(
            scan_is_publishable(None, &below_birthday, &below_birthday, |_| async {
                unreachable!()
            })
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn publication_keeps_the_last_snapshot_unless_the_scan_reached_its_target() {
        use zcash_client_backend::{
            data_api::{
                chain::ChainState,
                scanning::{ScanPriority, ScanRange},
            },
            proto::compact_formats::CompactBlock,
        };
        use zcash_primitives::block::BlockHash;

        let hash = |height: u64| BlockHash([height as u8; 32]).to_string();
        // the node's tip height and its block hash at each height.
        let node = Arc::new(Mutex::new((4_u64, (0..=6).map(hash).collect::<Vec<_>>())));
        let rpc_node = node.clone();
        let rpc = Router::new().route(
            "/",
            post(move |Json(request): Json<Value>| {
                let node = rpc_node.clone();
                async move {
                    let (tip, hashes) = node.lock().unwrap().clone();
                    let result = match request["method"].as_str().unwrap() {
                        "getblockchaininfo" => {
                            json!({"chain":"regtest","blocks":tip,"bestblockhash":hashes[tip as usize]})
                        }
                        "getblockhash" => {
                            json!(hashes[request["params"][0].as_u64().unwrap() as usize])
                        }
                        method => panic!("unexpected rpc {method}"),
                    };
                    Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result,"error":null}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, rpc).await.unwrap() });
        let (mut state, _dir) = state_with_local_wallet();
        Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(endpoint);

        let wallet = &state.0.wallet;
        wallet.update_chain_tip(4).await.unwrap();
        let blocks = (2..=4)
            .map(|height| CompactBlock {
                height,
                hash: vec![height as u8; 32],
                prev_hash: vec![(height - 1) as u8; 32],
                chain_metadata: Some(Default::default()),
                ..Default::default()
            })
            .collect();
        wallet
            .scan_batch(
                ScanRange::from_parts(2.into()..5.into(), ScanPriority::Historic),
                blocks,
                ChainState::empty(1.into(), BlockHash([1; 32])),
            )
            .await
            .unwrap();
        let target = |height| ChainCheckpoint {
            height,
            hash: hash(height),
        };

        // the scan reached its target and the tip moved on.
        node.lock().unwrap().0 = 6;
        state.refresh_wallet_snapshot(&target(4)).await.unwrap();
        let published = state.wallet_sync_status().await;
        assert_eq!(
            (published.fully_scanned_height, published.observed_height),
            (Some(4), Some(6))
        );
        state.0.wallet_snapshot.write().await.accounts[0].ironwood_zatoshi = 400_000_000;

        // short of the target, orphaned, and a tip that dropped below the target.
        for (target_height, tip, scanned_hash) in
            [(5, 6, hash(4)), (4, 6, "ee".repeat(32)), (6, 4, hash(4))]
        {
            {
                let mut node = node.lock().unwrap();
                node.0 = tip;
                node.1[4] = scanned_hash;
            }
            assert!(
                state
                    .refresh_wallet_snapshot(&target(target_height))
                    .await
                    .is_err()
            );
            let snapshot = state.0.wallet_snapshot.read().await;
            assert_eq!(snapshot.accounts[0].ironwood_zatoshi, 400_000_000);
            assert_eq!(
                (
                    snapshot.status.fully_scanned_height,
                    snapshot.status.observed_height,
                    snapshot.status.last_success_at,
                ),
                (
                    published.fully_scanned_height,
                    published.observed_height,
                    published.last_success_at,
                )
            );
        }
    }

    #[tokio::test]
    async fn synchronization_failure_preserves_the_last_good_snapshot() {
        let (state, _dir) = state_with_local_wallet();
        {
            let mut snapshot = state.0.wallet_snapshot.write().await;
            snapshot.accounts[0].ironwood_zatoshi = 400_000_000;
            snapshot.status.last_success_at = Some(123);
        }

        assert!(state.synchronize_wallet(Some(1)).await.is_err());

        let snapshot = state.0.wallet_snapshot.read().await;
        assert_eq!(snapshot.accounts[0].ironwood_zatoshi, 400_000_000);
        assert_eq!(snapshot.status.last_success_at, Some(123));
        assert_eq!(snapshot.status.state, "error");
        assert!(snapshot.status.error.is_some());
    }

    #[tokio::test]
    async fn send_rejects_invalid_pools_before_wallet_work() {
        let (state, _dir) = state_with_local_wallet();
        for (source_pool, destination_pool) in [("sapling", "ironwood"), ("ironwood", "sapling")] {
            let response = router(state.clone())
                .oneshot(
                    Request::post("/api/v1/send")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::to_vec(&json!({
                                "from_account": 1,
                                "to_account": 2,
                                "source_pool": source_pool,
                                "destination_pool": destination_pool,
                                "amount_zatoshi": 1,
                                "idempotency_key": "invalid-pool",
                            }))
                            .unwrap(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn failed_recovery_keeps_a_claim_with_a_wallet_journal() {
        let (state, dir) = state_with_local_wallet();
        let claim = state
            .0
            .store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();
        rusqlite::Connection::open(dir.path().join("wallet.db"))
            .unwrap()
            .execute(
                "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES(?1,?2,?3,?4)",
                rusqlite::params![claim.id, "txid", b"signed transaction", 140_u64],
            )
            .unwrap();

        discard_unprepared_claim(&state, &claim.id).await.unwrap();

        assert_eq!(
            state.0.store.activity_for_key("same").unwrap().unwrap().id,
            claim.id
        );
    }

    #[test]
    fn copies_spent_output_address_and_value_onto_vin() {
        let mut tx = json!({
            "vin": [{"txid": "aa", "vout": 1}],
            "vout": []
        });
        let mut prev = std::collections::HashMap::new();
        prev.insert(
            "aa".into(),
            json!({
                "vout": [
                    {"n": 0, "valueZat": 1},
                    {
                        "n": 1,
                        "valueZat": 50_000_000,
                        "scriptPubKey": {"addresses": ["tmABC"]}
                    }
                ]
            }),
        );
        attach_transparent_prevouts(&mut tx, &prev);
        assert_eq!(tx["vin"][0]["valueZat"], 50_000_000);
        assert_eq!(tx["vin"][0]["scriptPubKey"]["addresses"][0], "tmABC");
    }

    #[test]
    fn leaves_coinbase_inputs_untouched() {
        let mut tx = json!({"vin": [{"coinbase": "00"}]});
        attach_transparent_prevouts(&mut tx, &Default::default());
        assert_eq!(tx["vin"][0]["coinbase"], "00");
        assert!(tx["vin"][0].get("valueZat").is_none());
    }

    #[test]
    fn confirmed_block_hash_requires_confirmations_and_blockhash() {
        assert_eq!(
            confirmed_block_hash(&json!({
                "txid": "abc",
                "confirmations": 1,
                "blockhash": "0".repeat(64)
            })),
            Some("0000000000000000000000000000000000000000000000000000000000000000")
        );
        assert_eq!(
            confirmed_block_hash(&json!({
                "txid": "abc",
                "confirmations": 0,
                "blockhash": "0".repeat(64)
            })),
            None
        );
        assert_eq!(
            confirmed_block_hash(&json!({"txid": "abc", "confirmations": 3})),
            None
        );
        assert_eq!(
            confirmed_block_hash(&json!({"txid": "abc", "blockhash": "0".repeat(64)})),
            None
        );
        assert_eq!(
            confirmed_block_hash(&json!({
                "txid": "abc",
                "confirmations": 1,
                "blockhash": ""
            })),
            None
        );
        assert_eq!(confirmed_block_hash(&json!({"txid": "abc"})), None);
    }

    #[test]
    fn stale_confirmation_does_not_confirm_a_replacement_transaction() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let claimed = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();
        store
            .record_prepared(&claimed.id, "old-txid", b"old transaction", 140)
            .unwrap();
        let old = store.activity_for_key("same").unwrap().unwrap();
        store.reset_for_retry(&claimed.id, "old-txid").unwrap();
        store
            .record_prepared(&claimed.id, "new-txid", b"new transaction", 180)
            .unwrap();

        let current = apply_confirmation(
            &store,
            &old,
            &serde_json::json!({
                "confirmations": 1,
                "blockhash": "old-block",
            }),
        )
        .unwrap();

        assert_eq!(current.txid, "new-txid");
        assert_eq!(current.status, "prepared");
        assert_eq!(current.block_hash, None);
    }

    #[test]
    fn apply_confirmation_uses_node_blockhash_not_a_generate_hash() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let pending = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "issue-67", None)
            .unwrap();
        let pending = store
            .record_prepared(&pending.id, "txid-abc", b"raw transaction", 140)
            .unwrap();
        let pending = store.mark_broadcast(&pending.id, &pending.txid).unwrap();
        assert_eq!(pending.status, "broadcast");

        let generate_hash = "generate-hash-that-must-not-be-stored";
        let mempool = json!({"txid": "txid-abc", "confirmations": 0});
        let still = apply_confirmation(&store, &pending, &mempool).unwrap();
        assert_eq!(still.status, "broadcast");
        assert_eq!(still.block_hash, None);

        let mined = json!({
            "txid": "txid-abc",
            "confirmations": 1,
            "blockhash": "b".repeat(64)
        });
        let confirmed = apply_confirmation(&store, &pending, &mined).unwrap();
        let expected_hash = "b".repeat(64);
        assert_eq!(confirmed.status, "confirmed");
        assert_eq!(
            confirmed.block_hash.as_deref(),
            Some(expected_hash.as_str())
        );
        assert_ne!(confirmed.block_hash.as_deref(), Some(generate_hash));
        assert_eq!(confirmed.txid, "txid-abc");
    }

    #[test]
    fn reserves_the_treasury_account_from_public_operations() {
        for id in 1..=USER_ACCOUNT_COUNT {
            assert!(require_user_account(id).is_ok());
        }
        assert!(require_user_account(TREASURY_ACCOUNT_ID).is_err());
    }

    fn is_bad_request<T>(result: ApiResult<T>) -> bool {
        matches!(result, Err(error) if error.status == StatusCode::BAD_REQUEST)
    }

    #[test]
    fn copies_the_ironwood_root_from_the_treestate() {
        let mut block = json!({"hash": "ab", "finalorchardroot": "orchard-root"});
        add_ironwood_root(
            &mut block,
            &json!({"ironwood": {"commitments": {"finalRoot": "ironwood-root", "finalState": "00"}}}),
        );
        assert_eq!(block["finalironwoodroot"], "ironwood-root");
        assert_eq!(block["finalorchardroot"], "orchard-root");

        // At the genesis block Zakura returns empty Ironwood commitments.
        let mut block = json!({"hash": "ab"});
        add_ironwood_root(&mut block, &json!({"ironwood": {"commitments": {}}}));
        assert!(block.get("finalironwoodroot").is_none());
    }

    #[test]
    fn rejects_orchard_as_a_pool_after_nu6_3() {
        assert!(require_pool("ironwood", "pool").is_ok());
        assert!(require_pool("transparent", "pool").is_ok());
        let Err(error) = require_pool("orchard", "destination_pool") else {
            panic!("orchard must be rejected");
        };
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains("use ironwood"));
        assert!(is_bad_request(require_pool("sapling", "pool")));
    }

    #[test]
    fn absent_and_null_memos_differ_from_an_explicit_empty_memo() {
        let request = |memo: Option<Value>| {
            let mut body = json!({
                "from_account": 1, "to_account": 2, "source_pool": "ironwood",
                "destination_pool": "ironwood", "amount_zatoshi": 1,
                "idempotency_key": "memo-test-key",
            });
            if let Some(memo) = memo {
                body["memo"] = memo;
            }
            serde_json::from_value::<SendRequest>(body).expect("valid request shape")
        };
        assert_eq!(request(None).memo, None);
        assert_eq!(request(Some(Value::Null)).memo, None);
        assert_eq!(request(Some(json!(""))).memo.as_deref(), Some(""));

        assert!(matches!(parse_memo(None, "ironwood"), Ok(None)));
        assert!(matches!(parse_memo(None, "transparent"), Ok(None)));
        let Ok(Some(empty)) = parse_memo(Some(""), "ironwood") else {
            panic!("an explicit empty memo must be kept");
        };
        assert_eq!(empty.as_array(), &[0u8; 512]);
    }

    #[test]
    fn any_present_memo_is_rejected_for_a_transparent_destination() {
        assert!(is_bad_request(parse_memo(Some("hi"), "transparent")));
        assert!(is_bad_request(parse_memo(Some(""), "transparent")));
    }

    #[test]
    fn memos_are_bounded_by_utf8_bytes() {
        let Ok(Some(memo)) = parse_memo(Some("thanks for lunch"), "ironwood") else {
            panic!("expected an encoded memo");
        };
        assert_eq!(&memo.as_slice()[..16], b"thanks for lunch");
        assert!(memo.as_slice()[16..].iter().all(|byte| *byte == 0));

        assert!(parse_memo(Some(&"a".repeat(512)), "ironwood").is_ok());
        assert!(is_bad_request(parse_memo(
            Some(&"a".repeat(513)),
            "ironwood"
        )));

        // Three bytes per character: 170 fit (510 bytes), 171 do not (513).
        let Ok(Some(cjk)) = parse_memo(Some(&"桜".repeat(170)), "ironwood") else {
            panic!("510 bytes of multibyte text must fit");
        };
        assert_eq!(&cjk.as_slice()[..510], "桜".repeat(170).as_bytes());
        assert!(is_bad_request(parse_memo(
            Some(&"桜".repeat(171)),
            "ironwood"
        )));

        // Four bytes per character: exactly 512 fits, one more does not.
        assert!(parse_memo(Some(&"🌸".repeat(128)), "ironwood").is_ok());
        assert!(is_bad_request(parse_memo(
            Some(&"🌸".repeat(129)),
            "ironwood"
        )));
    }

    #[test]
    fn memos_must_not_end_with_nul() {
        assert!(is_bad_request(parse_memo(Some("hi\0"), "ironwood")));
        assert!(is_bad_request(parse_memo(Some("\0"), "ironwood")));
        assert!(parse_memo(Some("a\0b"), "ironwood").is_ok());
    }

    const REPLAY_KEY: &str = "existing-idempotency-key";

    /// A state with a real store and offline wallet whose node RPC refuses
    /// connections, so any synchronization or network access surfaces as a
    /// 500 rather than the 400 or replay these tests expect.
    fn offline_state() -> (AppState, tempfile::TempDir) {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        let state = AppState::new(store, wallet, "http://127.0.0.1:1".into(), "test".into());
        (state, dir)
    }

    fn valid_send() -> Value {
        json!({
            "from_account": 1,
            "to_account": 2,
            "source_pool": "ironwood",
            "destination_pool": "ironwood",
            "amount_zatoshi": 100_000,
            "idempotency_key": REPLAY_KEY,
        })
    }

    async fn post_send(state: &AppState, body: &Value) -> (StatusCode, Value) {
        let response = router(state.clone())
            .oneshot(
                Request::post("/api/v1/send")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn activity_ids(state: &AppState) -> Vec<String> {
        state
            .0
            .store
            .activities(100)
            .unwrap()
            .into_iter()
            .map(|activity| activity.id)
            .collect()
    }

    #[test]
    fn send_validation_allows_same_account_only_between_pools() {
        for to in [1, 2] {
            for source in ["ironwood", "transparent"] {
                for destination in ["ironwood", "transparent"] {
                    let mut body = valid_send();
                    body["to_account"] = json!(to);
                    body["source_pool"] = json!(source);
                    body["destination_pool"] = json!(destination);
                    let req = serde_json::from_value::<SendRequest>(body).unwrap();
                    let result = validate_send(&req);
                    if to == 1 && source == destination {
                        let error = result.unwrap_err();
                        assert_eq!(error.status, StatusCode::BAD_REQUEST);
                        assert_eq!(
                            error.message,
                            "choose a different account or a different destination pool"
                        );
                    } else {
                        assert!(
                            matches!(result, Ok(None)),
                            "{to}: {source} -> {destination}"
                        );
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn invalid_sends_are_rejected_before_replay_or_synchronization() {
        let (state, _dir) = offline_state();
        let original = state
            .0
            .store
            .claim_transfer(1, 2, "ironwood", "ironwood", 100_000, REPLAY_KEY, None)
            .unwrap();
        let before = activity_ids(&state);

        let with = |fields: &[(&str, Value)]| {
            let mut body = valid_send();
            for (field, value) in fields {
                body[*field] = value.clone();
            }
            body
        };
        let transparent = ("destination_pool", json!("transparent"));
        let cases = [
            ("short key", with(&[("idempotency_key", json!("short"))])),
            (
                "key with spaces",
                with(&[("idempotency_key", json!("has spaces here"))]),
            ),
            ("account 0", with(&[("from_account", json!(0))])),
            (
                "treasury account",
                with(&[("to_account", json!(TREASURY_ACCOUNT_ID))]),
            ),
            ("same account and pool", with(&[("to_account", json!(1))])),
            (
                "same account and transparent pool",
                with(&[
                    ("to_account", json!(1)),
                    ("source_pool", json!("transparent")),
                    ("destination_pool", json!("transparent")),
                ]),
            ),
            (
                "same account and unsupported source",
                with(&[("to_account", json!(1)), ("source_pool", json!("sapling"))]),
            ),
            (
                "same account and unsupported destination",
                with(&[
                    ("to_account", json!(1)),
                    ("destination_pool", json!("orchard")),
                ]),
            ),
            (
                "same account unshield with memo",
                with(&[
                    ("to_account", json!(1)),
                    ("destination_pool", json!("transparent")),
                    ("memo", json!("hi")),
                ]),
            ),
            (
                "bad source pool",
                with(&[("source_pool", json!("sapling"))]),
            ),
            (
                "bad destination pool",
                with(&[("destination_pool", json!("sprout"))]),
            ),
            ("zero amount", with(&[("amount_zatoshi", json!(0))])),
            (
                "amount over MAX_MONEY",
                with(&[("amount_zatoshi", json!(MAX_MONEY + 1))]),
            ),
            ("memo too long", with(&[("memo", json!("a".repeat(513)))])),
            ("memo ending in NUL", with(&[("memo", json!("hi\u{0}"))])),
            (
                "memo to transparent",
                with(&[transparent.clone(), ("memo", json!("hi"))]),
            ),
            (
                "empty memo to transparent",
                with(&[transparent, ("memo", json!(""))]),
            ),
        ];
        for (name, body) in cases {
            let (status, response) = post_send(&state, &body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {response}");
            assert_eq!(activity_ids(&state), before, "{name} changed activity");
        }
        assert_eq!(
            state
                .0
                .store
                .activity_for_key(REPLAY_KEY)
                .unwrap()
                .unwrap()
                .id,
            original.id
        );
    }

    #[tokio::test]
    async fn same_account_cross_pool_replays_preserve_activity_and_identity() {
        for (source, destination) in [("ironwood", "transparent"), ("transparent", "ironwood")] {
            let (state, _dir) = offline_state();
            let original = state
                .0
                .store
                .claim_transfer(1, 1, source, destination, 100_000, REPLAY_KEY, None)
                .unwrap();
            state
                .0
                .store
                .record_prepared(&original.id, "txid", b"signed transaction", 0)
                .unwrap();
            state.0.store.mark_broadcast(&original.id, "txid").unwrap();
            state
                .0
                .store
                .confirm(&original.id, "txid", &"c".repeat(64))
                .unwrap();
            let before = activity_ids(&state);
            let mut body = valid_send();
            body["to_account"] = json!(1);
            body["source_pool"] = json!(source);
            body["destination_pool"] = json!(destination);
            for _ in 0..2 {
                let (status, response) = post_send(&state, &body).await;
                assert_eq!(status, StatusCode::OK, "{response}");
                assert_eq!(response["id"], original.id);
                assert_eq!(response["txid"], "txid");
                assert_eq!(response["from_account"], 1);
                assert_eq!(response["to_account"], 1);
                assert_eq!(response["source_pool"], source);
                assert_eq!(response["destination_pool"], destination);
            }
            for changed in [("amount_zatoshi", json!(100_001)), ("to_account", json!(2))] {
                let mut conflict = body.clone();
                conflict[changed.0] = changed.1;
                assert_eq!(post_send(&state, &conflict).await.0, StatusCode::CONFLICT);
            }
            let mut reverse = body.clone();
            reverse["source_pool"] = json!(destination);
            reverse["destination_pool"] = json!(source);
            assert_eq!(post_send(&state, &reverse).await.0, StatusCode::CONFLICT);
            assert_eq!(activity_ids(&state), before);
        }
    }

    #[tokio::test]
    async fn a_valid_replay_returns_the_original_without_the_wallet_or_network() {
        let (state, _dir) = offline_state();
        let original = state
            .0
            .store
            .claim_transfer(1, 2, "ironwood", "ironwood", 100_000, REPLAY_KEY, None)
            .unwrap();
        state
            .0
            .store
            .record_prepared(&original.id, "txid", b"signed transaction", 0)
            .unwrap();
        state.0.store.mark_broadcast(&original.id, "txid").unwrap();
        state
            .0
            .store
            .confirm(&original.id, "txid", &"c".repeat(64))
            .unwrap();
        let before = activity_ids(&state);

        for memo in [None, Some(Value::Null)] {
            let mut body = valid_send();
            if let Some(memo) = memo {
                body["memo"] = memo;
            }
            let (status, response) = post_send(&state, &body).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            assert_eq!(response["id"], original.id);
            assert_eq!(response["txid"], "txid");
        }
        for memo in ["", "different memo"] {
            let mut body = valid_send();
            body["memo"] = json!(memo);
            let (status, _) = post_send(&state, &body).await;
            assert_eq!(status, StatusCode::CONFLICT);
        }
        assert_eq!(activity_ids(&state), before);

        // Guard for the harness itself: a send that is not a replay must reach
        // the (unreachable) network and fail, so the 200s above prove none did.
        let mut fresh = valid_send();
        fresh["idempotency_key"] = json!("a-key-never-used-before");
        let (status, _) = post_send(&state, &fresh).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(activity_ids(&state), before);
    }

    #[test]
    fn faucet_accepts_only_regtest_unified_and_transparent_addresses() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let account = store.account(1).unwrap();

        assert!(require_faucet_address(&account.unified_address).is_ok());
        assert!(require_faucet_address(&account.transparent_address).is_ok());
        assert!(require_faucet_address("not-an-address").is_err());
    }

    #[tokio::test]
    async fn address_faucet_rejects_missing_or_invalid_operation_keys_before_network_work() {
        let (state, _dir) = state_with_local_wallet();
        let address = state.0.store.account(1).unwrap().unified_address;
        for key in [None, Some("short"), Some("has spaces")] {
            let mut body = json!({"address": address, "amount_zatoshi": 1});
            if let Some(key) = key {
                body["idempotency_key"] = json!(key);
            }
            let response = router(state.clone())
                .oneshot(
                    Request::post("/api/v1/faucet/address")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn internal_address_faucet_replays_preserve_activity_after_store_reopen() {
        for pool in ["transparent", "ironwood"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("app.db");
            let store = Store::open(&path).unwrap();
            store.initialize().unwrap();
            let account = store.account(2).unwrap();
            let address = if pool == "transparent" {
                account.transparent_address
            } else {
                account.unified_address
            };
            let original = store.claim_faucet(2, pool, 123, "internal-replay").unwrap();
            store
                .record_prepared(&original.id, "original-txid", b"signed bytes", 140)
                .unwrap();
            store
                .confirm(&original.id, "original-txid", "original-block")
                .unwrap();
            drop(store);

            let store = Store::open(&path).unwrap();
            store.initialize().unwrap();
            let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
            let state = AppState::new(store, wallet, "http://127.0.0.1:1".into(), "test".into());
            let body = json!({"address": address, "amount_zatoshi": 123, "idempotency_key": "internal-replay"});
            for _ in 0..2 {
                let response = router(state.clone())
                    .oneshot(
                        Request::post("/api/v1/faucet/address")
                            .header("content-type", "application/json")
                            .body(Body::from(body.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let result: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(result["txid"], "original-txid");
                assert_eq!(result["block_hash"], "original-block");
                assert_eq!(result["status"], "confirmed");
            }
            for (field, value) in [
                ("amount_zatoshi", json!(124)),
                (
                    "address",
                    json!(state.0.store.account(3).unwrap().transparent_address),
                ),
            ] {
                let mut conflict = body.clone();
                conflict[field] = value;
                let response = router(state.clone())
                    .oneshot(
                        Request::post("/api/v1/faucet/address")
                            .header("content-type", "application/json")
                            .body(Body::from(conflict.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::CONFLICT);
            }
            let activities = state.0.store.activities(100).unwrap();
            assert_eq!(activities.len(), 1);
            assert_eq!(activities[0].id, original.id);
            assert_eq!(activities[0].txid, "original-txid");
        }
    }

    #[tokio::test]
    async fn confirmed_address_faucet_replays_without_sending_a_second_payment() {
        let (state, _dir) = state_with_local_wallet();
        let address = state.0.store.account(1).unwrap().unified_address;
        let claim = state
            .0
            .store
            .claim_address_faucet(&address, 123, "replay-address")
            .unwrap();
        state
            .0
            .store
            .record_address_prepared(&claim.id, "original-txid", b"original bytes", 140)
            .unwrap();
        state
            .0
            .store
            .confirm_address(&claim.id, "original-txid", "original-block")
            .unwrap();
        let body =
            json!({"address":address,"amount_zatoshi":123,"idempotency_key":"replay-address"});

        for _ in 0..2 {
            let response = router(state.clone())
                .oneshot(
                    Request::post("/api/v1/faucet/address")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let result: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(result["txid"], "original-txid");
            assert_eq!(result["block_hash"], "original-block");
            assert_eq!(result["status"], "confirmed");
        }
        let mut conflict = body;
        conflict["amount_zatoshi"] = json!(124);
        let response = router(state)
            .oneshot(
                Request::post("/api/v1/faucet/address")
                    .header("content-type", "application/json")
                    .body(Body::from(conflict.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn explorer_rejects_malformed_addresses_before_reaching_zakura() {
        let (state, _dir) = state_with_local_wallet();
        let account = state.0.store.account(1).unwrap();
        assert!(require_transparent_address(&account.transparent_address).is_ok());

        let response = router(state)
            .oneshot(
                Request::get("/api/v1/addresses/tmOOOOOOOOOOOOOOOOOOOOOOOO")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn explorer_reports_unknown_blocks_and_transactions_as_404() {
        // A node that knows nothing, answering with Zakura's not-found codes.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let node = format!("http://{}", listener.local_addr().unwrap());
        let not_found = Router::new().route(
            "/",
            post(|Json(req): Json<Value>| async move {
                let code = if req["method"] == "getblock" { -8 } else { -5 };
                Json(json!({"error": {"code": code, "message": "not found"}}))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, not_found).await });

        let (state, _dir) = state_with_local_wallet();
        let state = AppState::new(
            state.0.store.clone(),
            state.0.wallet.clone(),
            node,
            "test".into(),
        );
        let hash = "a".repeat(64);
        for path in [
            "/api/v1/blocks/999999".to_owned(),
            format!("/api/v1/transactions/{hash}"),
            "/api/v1/search?q=999999".to_owned(),
            format!("/api/v1/search?q={hash}"),
        ] {
            let response = router(state.clone())
                .oneshot(Request::get(&path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn search_does_not_mask_a_block_node_failure_as_404() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let node = format!("http://{}", listener.local_addr().unwrap());
        let failing = Router::new().route(
            "/",
            post(|Json(req): Json<Value>| async move {
                let code = if req["method"] == "getblock" { -28 } else { -5 };
                Json(json!({"error": {"code": code, "message": "injected"}}))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, failing).await });

        let (state, _dir) = state_with_local_wallet();
        let state = AppState::new(
            state.0.store.clone(),
            state.0.wallet.clone(),
            node,
            "t".into(),
        );
        let uri = format!("/api/v1/search?q={}", "a".repeat(64));
        let response = router(state)
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn api_error_logs_full_rpc_error_chain() {
        let error: RpcError =
            serde_json::from_value(json!({"code": -28, "message": "warming up"})).unwrap();
        let error = anyhow::Error::new(error)
            .context("Zakura getblock failed")
            .context("loading block details");
        let (response, logs) = capture_logs(|| ApiError::from(error).into_response());

        assert_eq!(logs.lines().count(), 1, "{logs}");
        assert!(logs.contains("ERROR"), "{logs}");
        assert!(
            logs.contains(
                "error=loading block details: Zakura getblock failed: RPC error -28: warming up"
            ),
            "{logs}"
        );
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"error": {"message": "loading block details", "status": 500}})
        );
    }

    #[tokio::test]
    async fn api_error_logs_skip_expected_failures() {
        for (error, status) in [
            (
                anyhow::Error::new(IdempotencyConflict),
                StatusCode::CONFLICT,
            ),
            (
                anyhow::Error::new(PaymentError::InsufficientFunds {
                    available: 100_000_000,
                    required: 100_010_000,
                }),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                anyhow::Error::new(PaymentError::TransparentMemo),
                StatusCode::BAD_REQUEST,
            ),
            (
                anyhow::Error::new(PaymentError::TreasuryExhausted),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            let error = error.context("payment could not complete");
            let (response, logs) = capture_logs(|| ApiError::from(error).into_response());
            assert!(logs.is_empty(), "{status}: {logs}");
            assert_eq!(response.status(), status);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap(),
                json!({"error": {"message": "payment could not complete", "status": status.as_u16()}})
            );
        }
    }

    #[tokio::test]
    async fn api_error_logs_skip_explorer_misses() {
        for code in [-5, -8] {
            let error: RpcError =
                serde_json::from_value(json!({"code": code, "message": "not found"})).unwrap();
            let error = anyhow::Error::new(error).context("Zakura getblock failed");
            let (response, logs) = capture_logs(|| not_found(error, NO_BLOCK).into_response());
            assert!(logs.is_empty(), "{code}: {logs}");
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap(),
                json!({"error": {"message": NO_BLOCK, "status": 404}})
            );
        }
    }

    #[test]
    fn a_dry_treasury_is_reported_as_unavailable() {
        assert_eq!(
            ApiError::from(anyhow::Error::new(PaymentError::TreasuryExhausted)).status,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn an_unaffordable_send_is_reported_as_a_client_error() {
        assert_eq!(
            ApiError::from(anyhow::Error::new(PaymentError::InsufficientFunds {
                available: 100_000_000,
                required: 100_010_000,
            }))
            .status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    /// An unknown pool is a client error, not the wallet's internal `bail!`
    /// surfacing as a 500 that the dashboard reports as an unexpected error.
    #[tokio::test]
    async fn send_quote_rejects_unknown_pools_as_bad_requests() {
        let (state, _dir) = state_with_local_wallet();
        let app = router(state);
        for (body, message) in [
            (
                json!({"from_account": 1, "source_pool": "sapling", "destination_pool": "ironwood"}),
                "source_pool must be transparent or ironwood",
            ),
            (
                json!({"from_account": 1, "source_pool": "ironwood", "destination_pool": "sapling"}),
                "destination_pool must be transparent or ironwood",
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/v1/send/quote")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(
                String::from_utf8_lossy(&bytes).contains(message),
                "expected {message} in {bytes:?}"
            );
        }
    }

    struct RecordingFaucetRuntime {
        events: Mutex<Vec<String>>,
        funds_available: AtomicBool,
        reward_discovered: AtomicBool,
        height: AtomicUsize,
        cursor: Mutex<Option<crate::db::TreasuryCursor>>,
    }

    impl Default for RecordingFaucetRuntime {
        fn default() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                funds_available: AtomicBool::new(false),
                reward_discovered: AtomicBool::new(false),
                height: AtomicUsize::new(2),
                cursor: Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl FaucetRuntime for RecordingFaucetRuntime {
        async fn prepare_payment(
            &self,
            _activity_id: Option<&str>,
            _seed: &str,
            _destination: &str,
            _amount_zatoshi: u64,
        ) -> anyhow::Result<PreparedPayment> {
            self.events.lock().unwrap().push("prepare".into());
            if self.funds_available.load(Ordering::SeqCst) {
                Ok(PreparedPayment {
                    txid: "recovered-txid".into(),
                    raw_transaction: b"recovered transaction".to_vec(),
                    expiry_height: 240,
                })
            } else {
                Err(anyhow::Error::new(PaymentError::InsufficientFunds {
                    available: 0,
                    required: 100_010_000,
                }))
            }
        }
        async fn chain_height(&self) -> anyhow::Result<u64> {
            Ok(self.height.load(Ordering::SeqCst) as u64)
        }
        async fn block_hash(&self, height: u32) -> anyhow::Result<String> {
            Ok(format!("{height:064x}"))
        }
        async fn synchronize_latest(&self) -> anyhow::Result<()> {
            self.events.lock().unwrap().push("sync".into());
            Ok(())
        }
        async fn treasury_cursor(&self) -> anyhow::Result<Option<crate::db::TreasuryCursor>> {
            Ok(self.cursor.lock().unwrap().clone())
        }
        async fn initialize_treasury_cursor(
            &self,
            receiver: &str,
            height: u32,
            hash: &str,
        ) -> anyhow::Result<crate::db::TreasuryCursor> {
            let cursor = crate::db::TreasuryCursor {
                receiver: receiver.into(),
                height,
                block_hash: hash.into(),
            };
            *self.cursor.lock().unwrap() = Some(cursor.clone());
            Ok(cursor)
        }
        async fn discover_reward(
            &self,
            cursor: &crate::db::TreasuryCursor,
            _treasury: &Account,
        ) -> anyhow::Result<crate::db::TreasuryCursor> {
            assert_eq!(cursor.height, 1);
            self.events.lock().unwrap().push("discover:2".into());
            self.reward_discovered.store(true, Ordering::SeqCst);
            let cursor = crate::db::TreasuryCursor {
                receiver: cursor.receiver.clone(),
                height: 2,
                block_hash: format!("{:064x}", 2),
            };
            *self.cursor.lock().unwrap() = Some(cursor.clone());
            Ok(cursor)
        }
        async fn shield_coinbase(
            &self,
            _seed: &str,
            _treasury: &Account,
            minimum_net: u64,
        ) -> anyhow::Result<()> {
            assert_eq!(minimum_net, 100_010_000);
            self.events.lock().unwrap().push("shield".into());
            if !self.reward_discovered.load(Ordering::SeqCst) {
                return Err(anyhow::Error::new(PaymentError::TreasuryExhausted));
            }
            self.funds_available.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn mine_and_sync(&self, blocks: u32) -> anyhow::Result<()> {
            self.events.lock().unwrap().push(format!("mine:{blocks}"));
            self.height.fetch_add(blocks as usize, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn insufficient_funds_discovers_only_the_needed_mature_reward_and_retries() {
        let runtime = RecordingFaucetRuntime::default();
        let treasury = Account {
            id: TREASURY_ACCOUNT_ID,
            name: "Account 6".into(),
            unified_address: "uregtest-treasury".into(),
            transparent_address: "tmTreasury".into(),
            unified_full_viewing_key: None,
            transparent_zatoshi: 0,
            ironwood_zatoshi: 0,
        };

        let prepared = prepare_with_replenishment(
            &runtime,
            None,
            "seed",
            &treasury,
            "uregtest-recipient",
            100_000_000,
        )
        .await
        .unwrap();
        assert_eq!(prepared.txid, "recovered-txid");
        assert_eq!(runtime.cursor.lock().unwrap().as_ref().unwrap().height, 2);
        assert_eq!(
            runtime.events.into_inner().unwrap(),
            [
                "prepare",
                "shield",
                "mine:99",
                "discover:2",
                "shield",
                "mine:1",
                "prepare"
            ]
        );
    }

    #[derive(Default)]
    struct RecordingPaymentSubmitter {
        broadcasts: Mutex<Vec<Vec<u8>>>,
        lookups: Mutex<VecDeque<Result<bool, &'static str>>>,
        recovered: Mutex<Option<PreparedPayment>>,
        height: AtomicU64,
        fail_next: AtomicBool,
    }

    #[tokio::test]
    async fn address_faucet_reconciles_the_original_txid_after_a_lost_broadcast_response() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.db");
        let store = Store::open(&path).unwrap();
        store.initialize().unwrap();
        let payment = store
            .claim_address_faucet("external-regtest-address", 12_000, "same-address-intent")
            .unwrap();
        store
            .record_address_prepared(&payment.id, "original-txid", b"original signed bytes", 140)
            .unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        store.initialize().unwrap();
        let payment = store
            .claim_address_faucet("external-regtest-address", 12_000, "same-address-intent")
            .unwrap();
        let runtime = RecordingPaymentSubmitter {
            lookups: Mutex::new(VecDeque::from([
                Ok(false),
                Err("node unavailable"),
                Ok(true),
            ])),
            height: AtomicU64::new(139),
            fail_next: AtomicBool::new(true),
            ..Default::default()
        };

        assert!(
            submit_address_prepared(&store, &runtime, &payment)
                .await
                .is_err()
        );
        let same = store
            .claim_address_faucet("external-regtest-address", 12_000, "same-address-intent")
            .unwrap();
        let resumed = submit_address_prepared(&store, &runtime, &same)
            .await
            .unwrap();
        let AddressSubmission::Broadcast(resumed) = resumed else {
            panic!("original payment must resume")
        };
        assert_eq!(resumed.txid, "original-txid");
        assert_eq!(resumed.status, "broadcast");
        assert_eq!(
            runtime.broadcasts.into_inner().unwrap(),
            [b"original signed bytes"]
        );
    }

    #[async_trait::async_trait]
    impl PaymentSubmitter for RecordingPaymentSubmitter {
        async fn transaction_known(&self, _txid: &str) -> anyhow::Result<bool> {
            self.lookups
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(false))
                .map_err(anyhow::Error::msg)
        }

        async fn chain_height(&self) -> anyhow::Result<u64> {
            Ok(self.height.load(Ordering::SeqCst))
        }

        async fn broadcast(&self, raw_transaction: &[u8]) -> anyhow::Result<()> {
            self.broadcasts
                .lock()
                .unwrap()
                .push(raw_transaction.to_vec());
            if self.fail_next.swap(false, Ordering::SeqCst) {
                anyhow::bail!("response lost");
            }
            Ok(())
        }

        async fn recover_prepared(&self, _txid: &str) -> anyhow::Result<Option<PreparedPayment>> {
            Ok(self.recovered.lock().unwrap().take())
        }
    }

    #[tokio::test]
    async fn retry_submits_the_same_prepared_transaction_after_a_lost_response() {
        for (to, source, destination) in [
            (2, "ironwood", "ironwood"),
            (1, "ironwood", "transparent"),
            (1, "transparent", "ironwood"),
        ] {
            let store = Store::open(":memory:").unwrap();
            store.initialize().unwrap();
            let claim = store
                .claim_transfer(1, to, source, destination, 12_000, "same", None)
                .unwrap();
            store
                .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
                .unwrap();
            let prepared = store.activity_for_key("same").unwrap().unwrap();
            let runtime = RecordingPaymentSubmitter {
                lookups: Mutex::new(VecDeque::from([
                    Ok(false),
                    Err("node unavailable"),
                    Ok(false),
                ])),
                height: AtomicU64::new(139),
                fail_next: AtomicBool::new(true),
                ..Default::default()
            };

            assert!(submit_prepared(&store, &runtime, &prepared).await.is_err());
            assert_eq!(
                store.activity_for_key("same").unwrap().unwrap().status,
                "prepared"
            );

            let broadcast = submit_prepared(&store, &runtime, &prepared).await.unwrap();
            assert!(matches!(broadcast, PreparedSubmission::Broadcast(_)));
            assert_eq!(
                runtime.broadcasts.into_inner().unwrap(),
                [b"signed transaction", b"signed transaction"]
            );
        }
    }

    #[tokio::test]
    async fn a_forgotten_broadcast_is_resubmitted_from_the_saved_bytes() {
        let (store, prepared) = prepared_payment(140);
        let broadcast = store.mark_broadcast(&prepared.id, &prepared.txid).unwrap();
        let runtime = RecordingPaymentSubmitter {
            lookups: Mutex::new(VecDeque::from([Ok(false)])),
            height: AtomicU64::new(139),
            ..Default::default()
        };

        let result = submit_prepared(&store, &runtime, &broadcast).await.unwrap();

        assert!(matches!(result, PreparedSubmission::Broadcast(_)));
        assert_eq!(
            runtime.broadcasts.into_inner().unwrap(),
            [b"signed transaction"]
        );
        assert_eq!(
            store.activity_for_key("same").unwrap().unwrap().status,
            "broadcast"
        );
    }

    #[tokio::test]
    async fn a_lookup_failure_does_not_broadcast_or_change_status() {
        let (store, prepared) = prepared_payment(140);
        let runtime = RecordingPaymentSubmitter {
            lookups: Mutex::new(VecDeque::from([Err("node unavailable")])),
            height: AtomicU64::new(139),
            ..Default::default()
        };

        assert!(submit_prepared(&store, &runtime, &prepared).await.is_err());
        assert!(runtime.broadcasts.into_inner().unwrap().is_empty());
        assert_eq!(
            store.activity_for_key("same").unwrap().unwrap().status,
            "prepared"
        );
    }

    #[tokio::test]
    async fn a_broadcast_error_is_success_when_the_transaction_is_known() {
        let (store, prepared) = prepared_payment(140);
        let runtime = RecordingPaymentSubmitter {
            lookups: Mutex::new(VecDeque::from([Ok(false), Ok(true)])),
            height: AtomicU64::new(139),
            fail_next: AtomicBool::new(true),
            ..Default::default()
        };

        let result = submit_prepared(&store, &runtime, &prepared).await.unwrap();

        assert!(matches!(result, PreparedSubmission::Broadcast(_)));
        assert_eq!(
            store.activity_for_key("same").unwrap().unwrap().status,
            "broadcast"
        );
    }

    #[tokio::test]
    async fn a_known_transaction_does_not_require_saved_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.db");
        let store = Store::open(&path).unwrap();
        store.initialize().unwrap();
        let claim = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();
        let prepared = store
            .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
            .unwrap();
        rusqlite::Connection::open(path)
            .unwrap()
            .execute("DELETE FROM prepared_payments", [])
            .unwrap();
        let runtime = RecordingPaymentSubmitter {
            lookups: Mutex::new(VecDeque::from([Ok(true)])),
            ..Default::default()
        };

        let result = submit_prepared(&store, &runtime, &prepared).await.unwrap();

        assert!(matches!(result, PreparedSubmission::Broadcast(_)));
        assert_eq!(
            store.activity_for_key("same").unwrap().unwrap().status,
            "broadcast"
        );
    }

    #[tokio::test]
    async fn a_legacy_broadcast_recovers_bytes_from_the_wallet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.db");
        let store = Store::open(&path).unwrap();
        store.initialize().unwrap();
        let claim = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();
        let prepared = store
            .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
            .unwrap();
        let broadcast = store.mark_broadcast(&prepared.id, &prepared.txid).unwrap();
        rusqlite::Connection::open(path)
            .unwrap()
            .execute("DELETE FROM prepared_payments", [])
            .unwrap();
        let runtime = RecordingPaymentSubmitter {
            lookups: Mutex::new(VecDeque::from([Ok(false)])),
            recovered: Mutex::new(Some(PreparedPayment {
                txid: broadcast.txid.clone(),
                raw_transaction: b"wallet transaction".to_vec(),
                expiry_height: 140,
            })),
            height: AtomicU64::new(139),
            ..Default::default()
        };

        let result = submit_prepared(&store, &runtime, &broadcast).await.unwrap();

        assert!(matches!(result, PreparedSubmission::Broadcast(_)));
        assert_eq!(
            runtime.broadcasts.into_inner().unwrap(),
            [b"wallet transaction"]
        );
        assert_eq!(
            store
                .prepared_transaction(&broadcast.id)
                .unwrap()
                .raw_transaction,
            b"wallet transaction"
        );
    }

    #[tokio::test]
    async fn an_expired_missing_transaction_is_reset_for_preparation() {
        let (store, prepared) = prepared_payment(140);
        let runtime = RecordingPaymentSubmitter {
            lookups: Mutex::new(VecDeque::from([Ok(false)])),
            height: AtomicU64::new(140),
            ..Default::default()
        };

        let result = submit_prepared(&store, &runtime, &prepared).await.unwrap();

        assert!(matches!(result, PreparedSubmission::Expired(_)));
        assert!(runtime.broadcasts.into_inner().unwrap().is_empty());
        let retry = store.activity_for_key("same").unwrap().unwrap();
        assert_eq!(retry.status, "preparing");
        assert!(retry.txid.is_empty());
    }

    #[tokio::test]
    async fn a_transaction_seen_at_expiry_is_not_replaced() {
        let (store, prepared) = prepared_payment(140);
        let runtime = RecordingPaymentSubmitter {
            lookups: Mutex::new(VecDeque::from([Ok(false), Ok(true)])),
            height: AtomicU64::new(140),
            ..Default::default()
        };

        let result = submit_prepared(&store, &runtime, &prepared).await.unwrap();

        assert!(matches!(result, PreparedSubmission::Broadcast(_)));
        let current = store.activity_for_key("same").unwrap().unwrap();
        assert_eq!(current.txid, "real-txid");
        assert_eq!(current.status, "broadcast");
    }

    fn prepared_payment(expiry_height: u64) -> (Store, Activity) {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let claim = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();
        store
            .record_prepared(&claim.id, "real-txid", b"signed transaction", expiry_height)
            .unwrap();
        let prepared = store.activity_for_key("same").unwrap().unwrap();
        (store, prepared)
    }
}
