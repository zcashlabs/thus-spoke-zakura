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
use tokio::sync::{Mutex, RwLock, broadcast};
use tower_http::{services::ServeDir, trace::TraceLayer};
use zcash_keys::address::Address;
use zcash_protocol::{consensus::COINBASE_MATURITY_BLOCKS, memo::MemoBytes, value::MAX_MONEY};

use crate::{
    db::{Account, Activity, Store, TREASURY_ACCOUNT_ID, USER_ACCOUNT_COUNT, ZATOSHIS_PER_ZEC},
    rpc::{ChainInfo, NodeRpc},
    wallet::{PaymentError, RealWallet, regtest_network},
};

#[derive(Clone)]
pub struct AppState(Arc<Inner>);
struct Inner {
    store: Store,
    wallet: RealWallet,
    rpc: NodeRpc,
    instance: String,
    events: broadcast::Sender<String>,
    wallet_sync: Mutex<()>,
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
    observed_height: Option<u64>,
    last_success_at: Option<u64>,
    error: Option<String>,
}

impl AppState {
    pub fn new(store: Store, wallet: RealWallet, rpc: String, instance: String) -> Self {
        let (events, _) = broadcast::channel(128);
        let accounts = store.accounts().unwrap_or_default();
        Self(Arc::new(Inner {
            store,
            wallet,
            rpc: NodeRpc::new(rpc),
            instance,
            events,
            wallet_sync: Mutex::new(()),
            wallet_snapshot: RwLock::new(WalletSnapshot {
                accounts,
                status: WalletSyncStatus {
                    state: "syncing",
                    fully_scanned_height: None,
                    observed_height: None,
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
        let _guard = self.0.wallet_sync.lock().await;
        if let Some(target) = target_height
            && self
                .0
                .wallet_snapshot
                .read()
                .await
                .status
                .fully_scanned_height
                .is_some_and(|height| height >= target)
        {
            return Ok(());
        }

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
                    .wait_for_height(target, Duration::from_secs(120))
                    .await?;
            }
            self.0.wallet.sync().await?;
            self.refresh_wallet_snapshot().await
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
        let observed = self.0.wallet.latest_height().await?;
        {
            let mut snapshot = self.0.wallet_snapshot.write().await;
            snapshot.status.observed_height = Some(observed);
        }
        self.synchronize_wallet(Some(observed)).await
    }

    async fn refresh_wallet_snapshot(&self) -> anyhow::Result<()> {
        let mut accounts = self.0.store.accounts()?;
        self.0.wallet.apply_balances(&mut accounts).await?;
        let (fully_scanned_height, chain_tip_height) = self.0.wallet.heights().await?;
        let changed = self.0.wallet_snapshot.read().await.accounts != accounts;
        {
            let mut snapshot = self.0.wallet_snapshot.write().await;
            snapshot.accounts = accounts;
            snapshot.status = WalletSyncStatus {
                state: "ready",
                fully_scanned_height,
                observed_height: chain_tip_height,
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
        let observed = self.0.wallet.latest_height().await?;
        {
            let mut snapshot = self.0.wallet_snapshot.write().await;
            snapshot.status.observed_height = Some(observed);
        }
        let scanned = self
            .0
            .wallet_snapshot
            .read()
            .await
            .status
            .fully_scanned_height;
        if scanned.is_none_or(|height| height < observed) {
            self.synchronize_wallet(Some(observed)).await?;
            notify(self, "chain");
        }
        Ok(())
    }
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
            tracing::warn!(%error, "background wallet synchronization failed");
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
    let static_dir = std::env::var("TSZ_WEB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("web/dist"));
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/status", get(status))
        .route("/api/v1/accounts", get(accounts))
        .route("/api/v1/activity", get(activity))
        .route("/api/v1/send", post(send))
        .route("/api/v1/faucet", post(faucet))
        .route("/api/v1/faucet/address", post(faucet_address))
        .route("/api/v1/mine", post(mine))
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
        account_count: state.0.store.user_accounts()?.len(),
        auto_mine: true,
        network: "Regtest",
        endpoints: PublicEndpoints {
            dashboard: format!("http://{dashboard_host}"),
            zakura_rpc: std::env::var("TSZ_PUBLIC_ZAKURA_RPC")
                .unwrap_or_else(|_| "http://127.0.0.1:18232".into()),
            lightwalletd: std::env::var("TSZ_PUBLIC_LIGHTWALLETD")
                .unwrap_or_else(|_| "http://127.0.0.1:9067".into()),
            p2p: std::env::var("TSZ_PUBLIC_P2P").unwrap_or_else(|_| "127.0.0.1:18233".into()),
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
    // Validate everything before the replay lookup so a malformed request is a
    // 400 even when it reuses an existing idempotency key.
    let memo = validate_send(&req)?;
    if let Some(existing) = state.0.store.activity_for_key(&req.idempotency_key)? {
        return Ok(Json(confirm_after_mining(&state, existing).await?));
    }
    state.synchronize_latest().await?;
    let destination = state.0.store.account(req.to_account)?;
    let address = if req.destination_pool == "orchard" {
        destination.unified_address
    } else {
        destination.transparent_address
    };
    let txid = state
        .0
        .wallet
        .send(
            &state.0.store.seed()?,
            req.from_account,
            &req.source_pool,
            &address,
            req.amount_zatoshi,
            memo,
        )
        .await?;
    let pending = state.0.store.transfer(
        req.from_account,
        req.to_account,
        &req.source_pool,
        &req.destination_pool,
        req.amount_zatoshi,
        &req.idempotency_key,
        &txid,
    )?;
    Ok(Json(confirm_after_mining(&state, pending).await?))
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
    if !matches!(req.pool.as_str(), "transparent" | "orchard") {
        return Err(ApiError::bad_request("pool must be transparent or orchard"));
    }
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
}

#[derive(Serialize)]
struct FaucetAddressResponse {
    address: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: String,
}

async fn faucet_address(
    State(state): State<AppState>,
    Json(req): Json<FaucetAddressRequest>,
) -> ApiResult<Json<FaucetAddressResponse>> {
    require_faucet_address(&req.address)?;
    if req.amount_zatoshi == 0 || req.amount_zatoshi > 5 * ZATOSHIS_PER_ZEC {
        return Err(ApiError::bad_request(
            "amount must be greater than zero and no more than 5 ZEC",
        ));
    }
    state.synchronize_latest().await?;
    let seed = state.0.store.seed()?;
    let treasury = state.0.store.account(TREASURY_ACCOUNT_ID)?;
    let txid =
        send_with_replenishment(&state, &seed, &treasury, &req.address, req.amount_zatoshi).await?;
    mine_and_sync(&state, 1).await?;
    let mined = state.0.rpc.transaction(&txid).await?;
    let block_hash = confirmed_block_hash(&mined)
        .context("faucet transaction was not included in a block")?
        .to_owned();
    Ok(Json(FaucetAddressResponse {
        address: req.address,
        amount_zatoshi: req.amount_zatoshi,
        txid,
        block_hash,
    }))
}

async fn fund_from_treasury(
    state: &AppState,
    account_id: u8,
    pool: &str,
    amount_zatoshi: u64,
    idempotency_key: &str,
) -> anyhow::Result<Activity> {
    if let Some(existing) = state.0.store.activity_for_key(idempotency_key)? {
        return confirm_after_mining(state, existing).await;
    }
    let destination = state.0.store.account(account_id)?;
    let address = match pool {
        "transparent" => destination.transparent_address,
        "orchard" => destination.unified_address,
        _ => anyhow::bail!("pool must be transparent or orchard"),
    };
    state.synchronize_latest().await?;
    let seed = state.0.store.seed()?;
    // SDK proposals check spendability and the actual fee before construction. Total
    // balances include pending change and cannot decide whether this request is fundable.
    let treasury = state.0.store.account(TREASURY_ACCOUNT_ID)?;
    let txid = send_with_replenishment(state, &seed, &treasury, &address, amount_zatoshi).await?;
    let pending = state
        .0
        .store
        .faucet(account_id, pool, amount_zatoshi, idempotency_key, &txid)?;
    confirm_after_mining(state, pending).await
}

#[derive(Deserialize)]
struct TreasuryOutput {
    txid: String,
    height: u32,
}

#[async_trait::async_trait]
trait FaucetRuntime: Sync {
    async fn send_payment(
        &self,
        seed: &str,
        destination: &str,
        amount_zatoshi: u64,
    ) -> anyhow::Result<String>;
    async fn chain_height(&self) -> anyhow::Result<u64>;
    async fn treasury_outputs(&self, address: &str) -> anyhow::Result<Vec<TreasuryOutput>>;
    async fn transaction(&self, txid: &str) -> anyhow::Result<Value>;
    async fn enhance_transaction(&self, raw: &str, height: u32) -> anyhow::Result<()>;
    async fn shield_coinbase(&self, seed: &str, treasury: &Account) -> anyhow::Result<()>;
    async fn mine_and_sync(&self, blocks: u32) -> anyhow::Result<()>;
}

#[async_trait::async_trait]
impl FaucetRuntime for AppState {
    async fn send_payment(
        &self,
        seed: &str,
        destination: &str,
        amount_zatoshi: u64,
    ) -> anyhow::Result<String> {
        self.0
            .wallet
            .send(
                seed,
                TREASURY_ACCOUNT_ID,
                "orchard",
                destination,
                amount_zatoshi,
                None,
            )
            .await
    }

    async fn chain_height(&self) -> anyhow::Result<u64> {
        Ok(self.0.rpc.chain_info().await?.blocks)
    }

    async fn treasury_outputs(&self, address: &str) -> anyhow::Result<Vec<TreasuryOutput>> {
        self.0
            .rpc
            .call("getaddressutxos", json!([{ "addresses": [address] }]))
            .await
    }

    async fn transaction(&self, txid: &str) -> anyhow::Result<Value> {
        self.0.rpc.transaction(txid).await
    }

    async fn enhance_transaction(&self, raw: &str, height: u32) -> anyhow::Result<()> {
        self.0.wallet.enhance_transaction(raw, height).await
    }

    async fn shield_coinbase(&self, seed: &str, treasury: &Account) -> anyhow::Result<()> {
        self.0
            .wallet
            .shield_coinbase(
                seed,
                TREASURY_ACCOUNT_ID,
                &treasury.transparent_address,
                &treasury.unified_address,
            )
            .await?;
        Ok(())
    }

    async fn mine_and_sync(&self, blocks: u32) -> anyhow::Result<()> {
        mine_and_sync(self, blocks).await?;
        Ok(())
    }
}

async fn send_with_replenishment<R: FaucetRuntime>(
    runtime: &R,
    seed: &str,
    treasury: &Account,
    destination: &str,
    amount_zatoshi: u64,
) -> anyhow::Result<String> {
    match runtime
        .send_payment(seed, destination, amount_zatoshi)
        .await
    {
        Err(error)
            if matches!(
                error.downcast_ref(),
                Some(PaymentError::InsufficientFunds { .. })
            ) =>
        {
            replenish_treasury(runtime, seed, treasury).await?;
            runtime
                .send_payment(seed, destination, amount_zatoshi)
                .await
                .map_err(|error| {
                    if matches!(
                        error.downcast_ref(),
                        Some(PaymentError::InsufficientFunds { .. })
                    ) {
                        anyhow::Error::new(PaymentError::TreasuryExhausted)
                    } else {
                        error
                    }
                })
        }
        result => result,
    }
}

async fn mature_treasury_outputs<R: FaucetRuntime>(
    runtime: &R,
    treasury: &Account,
) -> anyhow::Result<Vec<TreasuryOutput>> {
    let height = runtime.chain_height().await?;
    let mut outputs = runtime
        .treasury_outputs(&treasury.transparent_address)
        .await?;
    // The wallet birthday is block 2; block 1 is deliberately outside its scan.
    outputs.retain(|output| {
        output.height >= 2
            && u64::from(output.height) + u64::from(COINBASE_MATURITY_BLOCKS) <= height
    });
    Ok(outputs)
}

async fn replenish_treasury<R: FaucetRuntime>(
    runtime: &R,
    seed: &str,
    treasury: &Account,
) -> anyhow::Result<()> {
    let mut outputs = mature_treasury_outputs(runtime, treasury).await?;
    if outputs.is_empty() {
        runtime.mine_and_sync(102).await?;
        outputs = mature_treasury_outputs(runtime, treasury).await?;
    }
    // lightwalletd UTXOs omit tx_index, so enhance all mature rewards before
    // the SDK's coinbase-only selector evaluates them.
    for output in outputs {
        let tx = runtime.transaction(&output.txid).await?;
        if tx.pointer("/vin/0/coinbase").is_none() {
            continue;
        }
        let raw = tx
            .get("hex")
            .and_then(Value::as_str)
            .context("Zakura omitted coinbase transaction hex")?;
        runtime.enhance_transaction(raw, output.height).await?;
    }
    runtime.shield_coinbase(seed, treasury).await?;
    runtime.mine_and_sync(1).await?;
    Ok(())
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
    const INITIAL_FUNDING_KEY: &str = "startup-account-1-orchard-v1";
    if let Some(existing) = state.0.store.activity_for_key(INITIAL_FUNDING_KEY)?
        && existing.status == "confirmed"
    {
        return Ok(());
    }
    let seed = state.0.store.seed()?;
    let treasury = state.0.store.account(TREASURY_ACCOUNT_ID)?;
    if state
        .0
        .store
        .activity_for_key(INITIAL_FUNDING_KEY)?
        .is_none()
    {
        // A fresh wallet needs scanned blocks before a proposal can determine its target height.
        replenish_treasury(state, &seed, &treasury).await?;
    }
    fund_from_treasury(
        state,
        1,
        "orchard",
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
        account.orchard_zatoshi == 5 * ZATOSHIS_PER_ZEC,
        "Account 1 startup Orchard balance is {}, expected {} zatoshi; reset this existing instance to migrate to the hidden treasury",
        account.orchard_zatoshi,
        5 * ZATOSHIS_PER_ZEC
    );
    Ok(())
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
    let hashes = mine_and_sync(&state, req.blocks).await?;
    Ok(Json(json!({"blocks":hashes.len(),"hashes":hashes})))
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
    Ok(Json(state.0.rpc.block(&id).await?))
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
    let mut tx = state.0.rpc.transaction(&txid).await?;
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
    if let Ok(block) = state.0.rpc.block(&query.q).await {
        return Ok(Json(json!({"type":"block","value":block})));
    }
    let tx = state.0.rpc.transaction(&query.q).await?;
    Ok(Json(json!({"type":"transaction","value":tx})))
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
        Some(hash) => store.confirm(&pending.id, hash),
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
                %error,
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
                %error,
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
    if key.len() < 8 || key.len() > 128 {
        Err(ApiError::bad_request(
            "idempotency_key must contain 8-128 characters",
        ))
    } else if !key.bytes().all(|byte| byte.is_ascii_graphic()) {
        Err(ApiError::bad_request(
            "idempotency_key must contain only visible ASCII characters",
        ))
    } else {
        Ok(())
    }
}

fn require_pool(pool: &str, field: &str) -> ApiResult<()> {
    if matches!(pool, "transparent" | "orchard") {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "{field} must be transparent or orchard"
        )))
    }
}

/// Checks the whole send request without touching the store, wallet or node,
/// and returns the encoded memo it carries.
fn validate_send(req: &SendRequest) -> ApiResult<Option<MemoBytes>> {
    require_key(&req.idempotency_key)?;
    require_user_account(req.from_account)?;
    require_user_account(req.to_account)?;
    if req.from_account == req.to_account {
        return Err(ApiError::bad_request(
            "from_account and to_account must be different accounts",
        ));
    }
    require_pool(&req.source_pool, "source_pool")?;
    require_pool(&req.destination_pool, "destination_pool")?;
    if req.amount_zatoshi == 0 || req.amount_zatoshi > MAX_MONEY {
        return Err(ApiError::bad_request(format!(
            "amount_zatoshi must be between 1 and {MAX_MONEY}"
        )));
    }
    parse_memo(req.memo.as_deref(), &req.destination_pool)
}

/// An absent (or null) memo is `None`. Any present memo, including `""`, is
/// encoded as an explicit ZIP-302 text memo and is only valid for orchard.
fn parse_memo(memo: Option<&str>, destination_pool: &str) -> ApiResult<Option<MemoBytes>> {
    let Some(text) = memo else {
        return Ok(None);
    };
    if destination_pool != "orchard" {
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
impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            status: match error.downcast_ref() {
                Some(PaymentError::TreasuryExhausted) => StatusCode::SERVICE_UNAVAILABLE,
                Some(PaymentError::TransparentMemo) => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
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
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

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

    /// A misconfigured TSZ_WEB_DIR should fail visibly rather than serving an
    /// empty 200 that looks like a working dashboard.
    #[tokio::test]
    async fn a_missing_shell_is_reported_rather_than_served_empty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (status, _) = get(&dir, "/wallet").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
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
    async fn account_reads_return_the_snapshot_without_contacting_lightwalletd() {
        let (state, _dir) = state_with_local_wallet();
        {
            let mut snapshot = state.0.wallet_snapshot.write().await;
            snapshot.accounts[0].orchard_zatoshi = 400_000_000;
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
        assert_eq!(accounts[0]["orchard_zatoshi"], 400_000_000);
        let expected_keys = std::collections::BTreeSet::from([
            "id",
            "name",
            "unified_address",
            "transparent_address",
            "transparent_zatoshi",
            "orchard_zatoshi",
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
    async fn synchronization_failure_preserves_the_last_good_snapshot() {
        let (state, _dir) = state_with_local_wallet();
        {
            let mut snapshot = state.0.wallet_snapshot.write().await;
            snapshot.accounts[0].orchard_zatoshi = 400_000_000;
            snapshot.status.last_success_at = Some(123);
        }

        assert!(state.synchronize_wallet(Some(1)).await.is_err());

        let snapshot = state.0.wallet_snapshot.read().await;
        assert_eq!(snapshot.accounts[0].orchard_zatoshi, 400_000_000);
        assert_eq!(snapshot.status.last_success_at, Some(123));
        assert_eq!(snapshot.status.state, "error");
        assert!(snapshot.status.error.is_some());
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
    fn apply_confirmation_uses_node_blockhash_not_a_generate_hash() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let pending = store
            .transfer(1, 2, "orchard", "orchard", 12_000, "issue-67", "txid-abc")
            .unwrap();
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
    fn absent_and_null_memos_differ_from_an_explicit_empty_memo() {
        let request = |memo: Option<Value>| {
            let mut body = json!({
                "from_account": 1, "to_account": 2, "source_pool": "orchard",
                "destination_pool": "orchard", "amount_zatoshi": 1,
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

        assert!(matches!(parse_memo(None, "orchard"), Ok(None)));
        assert!(matches!(parse_memo(None, "transparent"), Ok(None)));
        let Ok(Some(empty)) = parse_memo(Some(""), "orchard") else {
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
        let Ok(Some(memo)) = parse_memo(Some("thanks for lunch"), "orchard") else {
            panic!("expected an encoded memo");
        };
        assert_eq!(&memo.as_slice()[..16], b"thanks for lunch");
        assert!(memo.as_slice()[16..].iter().all(|byte| *byte == 0));

        assert!(parse_memo(Some(&"a".repeat(512)), "orchard").is_ok());
        assert!(is_bad_request(parse_memo(
            Some(&"a".repeat(513)),
            "orchard"
        )));

        // Three bytes per character: 170 fit (510 bytes), 171 do not (513).
        let Ok(Some(cjk)) = parse_memo(Some(&"桜".repeat(170)), "orchard") else {
            panic!("510 bytes of multibyte text must fit");
        };
        assert_eq!(&cjk.as_slice()[..510], "桜".repeat(170).as_bytes());
        assert!(is_bad_request(parse_memo(
            Some(&"桜".repeat(171)),
            "orchard"
        )));

        // Four bytes per character: exactly 512 fits, one more does not.
        assert!(parse_memo(Some(&"🌸".repeat(128)), "orchard").is_ok());
        assert!(is_bad_request(parse_memo(
            Some(&"🌸".repeat(129)),
            "orchard"
        )));
    }

    #[test]
    fn memos_must_not_end_with_nul() {
        assert!(is_bad_request(parse_memo(Some("hi\0"), "orchard")));
        assert!(is_bad_request(parse_memo(Some("\0"), "orchard")));
        assert!(parse_memo(Some("a\0b"), "orchard").is_ok());
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
            "source_pool": "orchard",
            "destination_pool": "orchard",
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

    #[tokio::test]
    async fn invalid_sends_are_rejected_before_replay_or_synchronization() {
        let (state, _dir) = offline_state();
        let original = state
            .0
            .store
            .transfer(1, 2, "orchard", "orchard", 100_000, REPLAY_KEY, "txid")
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
            ("same account", with(&[("to_account", json!(1))])),
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
    async fn a_valid_replay_returns_the_original_without_the_wallet_or_network() {
        let (state, _dir) = offline_state();
        let original = state
            .0
            .store
            .transfer(1, 2, "orchard", "orchard", 100_000, REPLAY_KEY, "txid")
            .unwrap();
        let before = activity_ids(&state);

        for memo in [
            None,
            Some(Value::Null),
            Some(json!("")),
            Some(json!("hello")),
        ] {
            let mut body = valid_send();
            if let Some(memo) = memo {
                body["memo"] = memo;
            }
            let (status, response) = post_send(&state, &body).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            assert_eq!(response["id"], original.id);
            assert_eq!(response["txid"], "txid");
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

    #[test]
    fn a_dry_treasury_is_reported_as_unavailable() {
        assert_eq!(
            ApiError::from(anyhow::Error::new(PaymentError::TreasuryExhausted)).status,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[derive(Default)]
    struct RecordingFaucetRuntime {
        events: Mutex<Vec<String>>,
        funds_available: AtomicBool,
        height_checks: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl FaucetRuntime for RecordingFaucetRuntime {
        async fn send_payment(
            &self,
            _seed: &str,
            _destination: &str,
            _amount_zatoshi: u64,
        ) -> anyhow::Result<String> {
            self.events.lock().unwrap().push("send".into());
            if self.funds_available.load(Ordering::SeqCst) {
                Ok("recovered-txid".into())
            } else {
                Err(anyhow::Error::new(PaymentError::InsufficientFunds {
                    available: 0,
                    required: 100_010_000,
                }))
            }
        }

        async fn chain_height(&self) -> anyhow::Result<u64> {
            let check = self.height_checks.fetch_add(1, Ordering::SeqCst);
            let height = if check == 0 { 101 } else { 203 };
            self.events.lock().unwrap().push(format!("height:{height}"));
            Ok(height)
        }

        async fn treasury_outputs(&self, _address: &str) -> anyhow::Result<Vec<TreasuryOutput>> {
            self.events.lock().unwrap().push("outputs".into());
            Ok(vec![TreasuryOutput {
                txid: "coinbase-txid".into(),
                height: 2,
            }])
        }

        async fn transaction(&self, txid: &str) -> anyhow::Result<Value> {
            self.events
                .lock()
                .unwrap()
                .push(format!("transaction:{txid}"));
            Ok(json!({"vin":[{"coinbase":"00"}],"hex":"raw-coinbase"}))
        }

        async fn enhance_transaction(&self, raw: &str, height: u32) -> anyhow::Result<()> {
            self.events
                .lock()
                .unwrap()
                .push(format!("enhance:{raw}:{height}"));
            Ok(())
        }

        async fn shield_coinbase(&self, _seed: &str, _treasury: &Account) -> anyhow::Result<()> {
            self.events.lock().unwrap().push("shield".into());
            self.funds_available.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn mine_and_sync(&self, blocks: u32) -> anyhow::Result<()> {
            self.events.lock().unwrap().push(format!("mine:{blocks}"));
            Ok(())
        }
    }

    #[tokio::test]
    async fn insufficient_funds_replenishes_from_mature_rewards_and_retries() {
        let runtime = RecordingFaucetRuntime::default();
        let treasury = Account {
            id: TREASURY_ACCOUNT_ID,
            name: "Account 6".into(),
            unified_address: "uregtest-treasury".into(),
            transparent_address: "tmTreasury".into(),
            unified_full_viewing_key: None,
            transparent_zatoshi: 0,
            orchard_zatoshi: 0,
        };

        let txid = send_with_replenishment(
            &runtime,
            "seed",
            &treasury,
            "uregtest-recipient",
            100_000_000,
        )
        .await
        .unwrap();

        assert_eq!(txid, "recovered-txid");
        assert_eq!(
            runtime.events.into_inner().unwrap(),
            [
                "send",
                "height:101",
                "outputs",
                "mine:102",
                "height:203",
                "outputs",
                "transaction:coinbase-txid",
                "enhance:raw-coinbase:2",
                "shield",
                "mine:1",
                "send",
            ]
        );
    }
}
