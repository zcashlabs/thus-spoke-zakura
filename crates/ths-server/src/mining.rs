use crate::rpc::{ChainCheckpoint, RpcError};
use crate::state::{AppState, notify};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde::Serialize;
use tokio::sync::{Mutex, RwLock, watch};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MiningState {
    Mining,
    Syncing,
    Completed,
    Failed,
}

/// How an instance produces automatic blocks. One mode is active for the whole
/// server lifetime; it is chosen at startup and never changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MiningMode {
    /// Every payment, faucet, and treasury operation mines its own confirming block.
    Auto,
    /// A background task mines an empty block every `block_time` seconds.
    Interval,
    /// A background task mines when the mempool has a new, nonempty transaction set.
    Mempool,
    /// No automatic blocks; unresolved work waits for an explicit `ths mine`.
    Manual,
}

impl MiningMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Interval => "interval",
            Self::Mempool => "mempool",
            Self::Manual => "manual",
        }
    }

    /// Whether the mode mines a block as part of a payment operation. Interval
    /// and mempool modes still produce blocks, but from a background task.
    pub fn auto_mine(self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// The instance's mining configuration, decoded from the launcher's environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MiningConfig {
    pub mode: MiningMode,
    pub block_time: Option<u64>,
}

impl Default for MiningConfig {
    fn default() -> Self {
        Self {
            mode: MiningMode::Auto,
            block_time: None,
        }
    }
}

impl MiningConfig {
    /// Decodes `THS_MINING_MODE` and `THS_BLOCK_TIME`. An absent mode is
    /// `auto`, preserving the pre-existing behavior for older launchers.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_values(
            std::env::var("THS_MINING_MODE").ok().as_deref(),
            std::env::var("THS_BLOCK_TIME").ok().as_deref(),
        )
    }

    fn from_values(mode: Option<&str>, block_time: Option<&str>) -> anyhow::Result<Self> {
        let mode = match mode {
            None | Some("") | Some("auto") => MiningMode::Auto,
            Some("interval") => MiningMode::Interval,
            Some("mempool") => MiningMode::Mempool,
            Some("manual") => MiningMode::Manual,
            Some(other) => anyhow::bail!("unsupported THS_MINING_MODE {other:?}"),
        };
        let block_time = match block_time {
            Some(value) => Some(
                value
                    .parse::<u64>()
                    .with_context(|| format!("invalid THS_BLOCK_TIME {value:?}"))?,
            ),
            None => None,
        };
        Self { mode, block_time }.validate()
    }

    fn validate(self) -> anyhow::Result<Self> {
        match (self.mode, self.block_time) {
            (MiningMode::Interval, Some(seconds)) if seconds > 0 => Ok(self),
            (MiningMode::Interval, _) => {
                anyhow::bail!("interval mining requires a positive THS_BLOCK_TIME")
            }
            (_, Some(_)) => {
                anyhow::bail!("THS_BLOCK_TIME is only valid with interval mining")
            }
            _ => Ok(self),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MiningJob {
    pub id: String,
    pub requested_blocks: u32,
    pub completed_blocks: u32,
    pub state: MiningState,
    pub error: Option<String>,
    pub progress_uncertain: bool,
    /// Only set once the job reaches `Completed`: both mining and wallet
    /// synchronization succeeded, so the mined hashes are final.
    pub hashes: Option<Vec<String>>,
}

#[async_trait::async_trait]
pub trait MiningRuntime: Send + Sync + 'static {
    async fn generate_one(&self) -> anyhow::Result<String>;
    async fn synchronize(&self, tip_hash: &str) -> anyhow::Result<()>;
    fn notify(&self, topic: &'static str);
}

#[derive(Clone)]
pub struct MiningUpdate {
    pub job: MiningJob,
    pub hashes: Option<Arc<Vec<String>>>,
}

pub struct MiningCoordinator {
    state: Mutex<CoordinatorState>,
    pub(crate) chain_operations: Arc<RwLock<()>>,
    capacity: usize,
}

struct CoordinatorState {
    jobs: HashMap<String, MiningJob>,
    keys: HashMap<String, String>,
    latest_id: Option<String>,
    active_id: Option<String>,
    latest_updates: Option<watch::Sender<MiningUpdate>>,
}

pub struct Admission {
    pub job: MiningJob,
    pub updates: watch::Receiver<MiningUpdate>,
}

#[derive(Debug, thiserror::Error)]
pub enum MiningAdmissionError {
    #[error("blocks must be between 1 and 10000")]
    InvalidBlocks,
    #[error("idempotency_key must contain 8-128 visible ASCII characters without spaces")]
    InvalidKey,
    #[error("idempotency_key was already used with a different block count")]
    KeyConflict,
    #[error("A manual mining job is already active")]
    Busy,
    #[error("A chain operation is already active")]
    ChainBusy,
    #[error(
        "Mining job capacity reached (1024 jobs per server lifetime); existing jobs remain available"
    )]
    Capacity,
}

pub(crate) fn validate_idempotency_key(key: &str) -> Result<(), &'static str> {
    if !(8..=128).contains(&key.len()) {
        Err("idempotency_key must contain 8-128 characters")
    } else if !key.bytes().all(|byte| byte.is_ascii_graphic()) {
        Err("idempotency_key must contain only visible ASCII characters")
    } else {
        Ok(())
    }
}

impl MiningCoordinator {
    pub fn new() -> Self {
        Self {
            chain_operations: Arc::new(RwLock::new(())),
            state: Mutex::new(CoordinatorState {
                jobs: HashMap::new(),
                keys: HashMap::new(),
                latest_id: None,
                active_id: None,
                latest_updates: None,
            }),
            capacity: 1024,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            ..Self::new()
        }
    }

    pub async fn start(
        self: &Arc<Self>,
        runtime: Arc<dyn MiningRuntime>,
        blocks: u32,
        key: String,
    ) -> Result<Admission, MiningAdmissionError> {
        if !(1..=10_000).contains(&blocks) {
            return Err(MiningAdmissionError::InvalidBlocks);
        }
        if validate_idempotency_key(&key).is_err() {
            return Err(MiningAdmissionError::InvalidKey);
        }
        let mut state = self.state.lock().await;
        if let Some(id) = state.keys.get(&key) {
            let job = state
                .jobs
                .get(id)
                .expect("Every mining key has a snapshot")
                .clone();
            if job.requested_blocks != blocks {
                return Err(MiningAdmissionError::KeyConflict);
            }
            let updates = if state.latest_id.as_ref() == Some(id) {
                state
                    .latest_updates
                    .as_ref()
                    .expect("Latest mining job has a channel")
                    .subscribe()
            } else {
                watch::channel(MiningUpdate {
                    job: job.clone(),
                    hashes: None,
                })
                .1
            };
            return Ok(Admission { job, updates });
        }
        if state.active_id.is_some() {
            return Err(MiningAdmissionError::Busy);
        }
        let chain = self
            .chain_operations
            .clone()
            .try_read_owned()
            .map_err(|_| MiningAdmissionError::ChainBusy)?;
        if state.jobs.len() >= self.capacity {
            return Err(MiningAdmissionError::Capacity);
        }
        let job = MiningJob {
            id: uuid::Uuid::new_v4().to_string(),
            requested_blocks: blocks,
            completed_blocks: 0,
            state: MiningState::Mining,
            error: None,
            progress_uncertain: false,
            hashes: None,
        };
        let (sender, updates) = watch::channel(MiningUpdate {
            job: job.clone(),
            hashes: None,
        });
        state.keys.insert(key, job.id.clone());
        state.jobs.insert(job.id.clone(), job.clone());
        state.latest_id = Some(job.id.clone());
        state.active_id = Some(job.id.clone());
        state.latest_updates = Some(sender);

        // Both tasks are detached from the request; only the supervisor owns the worker handle.
        let worker_jobs = Arc::clone(self);
        let worker_runtime = Arc::clone(&runtime);
        let worker_job = job.clone();
        let worker = tokio::spawn(async move {
            let _chain = chain;
            worker_jobs.run(worker_runtime, worker_job).await;
        });
        let supervisor_jobs = Arc::clone(self);
        let id = job.id.clone();
        tokio::spawn(async move {
            if let Err(error) = worker.await {
                tracing::error!(job_id = %id, %error, "Mining worker failed");
                let mut job = supervisor_jobs
                    .get(&id)
                    .await
                    .expect("Worker job has a snapshot");
                if !matches!(job.state, MiningState::Completed | MiningState::Failed) {
                    job.state = MiningState::Failed;
                    job.progress_uncertain = true;
                    job.error = Some(format!("Mining worker failed: {error}"));
                    supervisor_jobs
                        .publish(MiningUpdate { job, hashes: None })
                        .await;
                    runtime.notify("mining");
                    runtime.notify("chain");
                }
            }
        });
        Ok(Admission { job, updates })
    }

    pub async fn latest(&self) -> Option<MiningJob> {
        let state = self.state.lock().await;
        state
            .latest_id
            .as_ref()
            .and_then(|id| state.jobs.get(id))
            .cloned()
    }

    pub async fn get(&self, id: &str) -> Option<MiningJob> {
        self.state.lock().await.jobs.get(id).cloned()
    }

    /// A manual job is admitted but not yet terminal. Background mining skips
    /// while one is active so its blocks never interleave with a manual count.
    pub async fn is_active(&self) -> bool {
        self.state.lock().await.active_id.is_some()
    }

    async fn publish(&self, update: MiningUpdate) {
        let sender = {
            let mut state = self.state.lock().await;
            state.jobs.insert(update.job.id.clone(), update.job.clone());
            if matches!(
                update.job.state,
                MiningState::Completed | MiningState::Failed
            ) && state.active_id.as_ref() == Some(&update.job.id)
            {
                state.active_id = None;
            }
            if state.latest_id.as_ref() == Some(&update.job.id) {
                state.latest_updates.clone()
            } else {
                None
            }
        };
        if let Some(sender) = sender {
            // Publication must succeed even after every HTTP waiter disconnects.
            sender.send_replace(update);
        }
    }

    async fn run(&self, runtime: Arc<dyn MiningRuntime>, mut job: MiningJob) {
        let mut hashes = Vec::with_capacity(job.requested_blocks as usize);
        let generation: anyhow::Result<()> = async {
            for _ in 0..job.requested_blocks {
                hashes.push(runtime.generate_one().await?);
                job.completed_blocks = hashes.len() as u32;
                self.publish(MiningUpdate {
                    job: job.clone(),
                    hashes: None,
                })
                .await;
                runtime.notify("mining");
            }
            Ok(())
        }
        .await;
        if let Err(error) = generation {
            job.state = MiningState::Failed;
            job.progress_uncertain = true;
            job.error = Some(format!("Mining failed: {error:#}"));
            self.publish(MiningUpdate { job, hashes: None }).await;
            runtime.notify("mining");
            runtime.notify("chain");
            return;
        }
        job.state = MiningState::Syncing;
        self.publish(MiningUpdate {
            job: job.clone(),
            hashes: None,
        })
        .await;
        runtime.notify("mining");
        runtime.notify("chain");
        let synchronization: anyhow::Result<()> = async {
            let tip = hashes.last().context("Mining returned no hashes")?;
            runtime.synchronize(tip).await
        }
        .await;
        let hashes = match synchronization {
            Ok(()) => {
                job.state = MiningState::Completed;
                job.hashes = Some(hashes.clone());
                Some(Arc::new(hashes))
            }
            Err(error) => {
                job.state = MiningState::Failed;
                job.error = Some(format!(
                    "Blocks mined, but wallet synchronization failed: {error:#}"
                ));
                None
            }
        };
        self.publish(MiningUpdate { job, hashes }).await;
        runtime.notify("mining");
    }
}

#[cfg(test)]
#[path = "../tests/unit/mining/tests.rs"]
mod tests;

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
    pub(crate) async fn start_mining(
        &self,
        blocks: u32,
        key: String,
    ) -> Result<Admission, MiningAdmissionError> {
        let runtime: Arc<dyn MiningRuntime> = Arc::new(ManualMiningAdapter(self.clone()));
        #[cfg(test)]
        let runtime = self.0.mining_runtime.clone().unwrap_or(runtime);
        self.0.mining.start(runtime, blocks, key).await
    }

    /// Mines one block outside the manual coordinator and synchronizes the
    /// wallet. Used by the interval and mempool background loops, so it never
    /// counts toward a manual mining job.
    ///
    /// Returns `Err(BackgroundMineError::Generation(_))` when no block was
    /// produced: either the pre-generate checkpoint was unavailable (generate
    /// was not called), or `generate` failed with an explicit JSON-RPC error.
    /// Returns `Err(BackgroundMineError::Unknown { before, .. })` when the
    /// outcome is uncertain; `before` is the chain tip read inside the chain
    /// lock immediately before `generate` and is always valid. Returns
    /// `Err(BackgroundMineError::Sync(_))` when a block was produced but a
    /// subsequent step failed (do not retry generation with the same set).
    pub(crate) async fn mine_and_sync_background(&self) -> Result<(), BackgroundMineError> {
        // Hold the chain operations read lock across the checkpoint read,
        // generation, and sync so that reorg/rollback (which holds the write
        // lock) cannot change the chain tip between this function's checkpoint
        // read and the generate call.
        let _chain = self.0.mining.chain_operations.read().await;
        // Read the pre-generate checkpoint while holding the lock. If this
        // fails, skip the attempt entirely: without a valid baseline we cannot
        // reconcile a lost generate reply, so it is safer to retry on the next
        // tick than to call generate with no way to detect duplication.
        let before = self
            .0
            .rpc
            .checkpoint()
            .await
            .map_err(BackgroundMineError::Generation)?;
        let mut hashes = match self.0.rpc.generate(1).await {
            Ok(hashes) => hashes,
            Err(error) => {
                // Only an explicit JSON-RPC error from the node proves the
                // request was rejected before execution. Any HTTP status from a
                // proxy (including 429, 502, 503, 504) does not: proxies can
                // return these codes after forwarding the request and failing to
                // read or receive the upstream response.
                return Err(if error.downcast_ref::<RpcError>().is_some() {
                    BackgroundMineError::Generation(error)
                } else {
                    BackgroundMineError::Unknown { error, before }
                });
            }
        };
        if hashes.len() != 1 || hashes[0].is_empty() {
            // A success response with an unusable hash does not prove no block
            // was produced; the node may have generated before returning it.
            return Err(BackgroundMineError::Unknown {
                error: anyhow::anyhow!("Zakura returned an unusable generation acknowledgment"),
                before,
            });
        }
        let tip_hash = hashes.remove(0);
        let height = self
            .0
            .rpc
            .block(&tip_hash)
            .await
            .and_then(|b| {
                b.get("height")
                    .and_then(Value::as_u64)
                    .context("Zakura mined block omitted its height")
            })
            .map_err(BackgroundMineError::Sync)?;
        self.synchronize_wallet(Some(height))
            .await
            .map_err(BackgroundMineError::Sync)?;
        notify(self, "chain");
        Ok(())
    }
}

/// Error from a background mine attempt, distinguishing whether a block was
/// produced. Used by the interval and mempool loops to decide whether the
/// same transaction set may be retried.
#[derive(Debug)]
pub(crate) enum BackgroundMineError {
    /// `generate` was not called, or failed with an explicit JSON-RPC error
    /// from the node. No block was produced. Safe to retry with the same set.
    ///
    /// Also returned when the pre-generate checkpoint read fails: without a
    /// valid baseline the outcome of a lost generate reply cannot be
    /// reconciled, so the attempt is skipped entirely.
    Generation(anyhow::Error),
    /// `generate` was called but its reply was lost or unusable. The node may
    /// or may not have produced a block. `before` is the chain tip captured
    /// inside the chain lock immediately before `generate`; compare it against
    /// the current tip to decide whether to retry the same set.
    Unknown {
        error: anyhow::Error,
        before: ChainCheckpoint,
    },
    /// A block was produced but a subsequent step (block-metadata fetch or
    /// wallet sync) failed. Do not retry generation with the same transaction set.
    Sync(anyhow::Error),
}

/// Mines an empty block every `block_time` once startup has finished. Interval
/// mining is intentional block production, not a reaction to pending work.
pub(crate) async fn interval_mining_loop(state: AppState, block_time: Duration) {
    let mut interval = tokio::time::interval(block_time);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if !state.started() || state.0.mining.is_active().await {
            continue;
        }
        match state.mine_and_sync_background().await {
            Ok(()) => {}
            Err(BackgroundMineError::Generation(error)) => {
                tracing::warn!(
                    error = %format_args!("{error:#}"),
                    "background interval mining failed to generate a block"
                );
            }
            Err(BackgroundMineError::Unknown { error, .. }) => {
                tracing::warn!(
                    error = %format_args!("{error:#}"),
                    "background interval mining lost a generate reply; outcome unknown"
                );
            }
            Err(BackgroundMineError::Sync(error)) => {
                tracing::warn!(
                    error = %format_args!("{error:#}"),
                    "background interval mining produced a block but wallet sync failed"
                );
            }
        }
    }
}

/// The sorted transaction set to mine, or `None` when there is nothing new.
/// An empty mempool is nothing to do; a nonempty set identical to `attempted`
/// is skipped so a transaction the node cannot include does not produce
/// repeated empty blocks. This does not record the set: the caller records it
/// only once it commits to mining, so an in-flight manual job cannot swallow it.
fn new_mempool_set(attempted: Option<&[String]>, mempool: &mut [String]) -> Option<Vec<String>> {
    if mempool.is_empty() {
        return None;
    }
    mempool.sort_unstable();
    if attempted == Some(mempool) {
        return None;
    }
    Some(mempool.to_vec())
}

/// Mines a block when the mempool holds a transaction set it has not yet tried
/// to include. An unchanged, still-nonempty mempool is left alone.
pub(crate) async fn mempool_mining_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut attempted: Option<Vec<String>> = None;
    // An Unknown outcome waiting for reconciliation. Holds the pre-generate
    // chain checkpoint (always valid, captured inside the chain lock) and the
    // transaction set. On the next tick the current tip is compared against
    // `before`: if they differ the attempt produced a block and the set is
    // suppressed; if they match no block was produced and the set is retried.
    let mut unresolved: Option<(ChainCheckpoint, Vec<String>)> = None;
    loop {
        interval.tick().await;
        if !state.started() {
            continue;
        }

        // Reconcile any Unknown outcome from the previous tick before mining.
        if let Some((before, set)) = unresolved.take() {
            match state.0.rpc.checkpoint().await {
                Ok(current) => {
                    if before != current {
                        // Chain advanced from the pre-generate baseline: the
                        // attempt produced a block. Suppress retry for this set.
                        attempted = Some(set);
                        tracing::warn!(
                            "background mempool mining reconciled a lost generate \
                             reply as produced; suppressing retry"
                        );
                    } else {
                        // Tip unchanged since before generate: no block was
                        // produced; safe to retry.
                        tracing::warn!(
                            "background mempool mining reconciled a lost generate \
                             reply as not produced; retrying"
                        );
                    }
                }
                Err(error) => {
                    // Checkpoint still unavailable; carry the attempt forward.
                    unresolved = Some((before, set));
                    tracing::warn!(
                        error = %format_args!("{error:#}"),
                        "background mempool mining could not reconcile a lost \
                         generate reply; deferring to next tick"
                    );
                }
            }
            continue;
        }

        let mut mempool = match state.0.rpc.mempool().await {
            Ok(mempool) => mempool,
            Err(error) => {
                tracing::warn!(
                    error = %format_args!("{error:#}"),
                    "background mempool mining could not read the mempool"
                );
                continue;
            }
        };
        if mempool.is_empty() {
            attempted = None;
            continue;
        }
        let Some(set) = new_mempool_set(attempted.as_deref(), &mut mempool) else {
            continue;
        };
        if state.0.mining.is_active().await {
            continue;
        }
        // mine_and_sync_background acquires the chain lock, reads the pre-
        // generate checkpoint inside that lock, and embeds it in any Unknown
        // outcome. The checkpoint is always valid when Unknown is returned.
        match state.mine_and_sync_background().await {
            Ok(()) => {
                // Block produced and synced; record the set so an unchanged
                // mempool is skipped until a new transaction arrives.
                attempted = Some(set);
            }
            Err(BackgroundMineError::Generation(error)) => {
                // No block was produced (checkpoint unavailable or explicit
                // node rejection); do not record the set so the same
                // transactions are retried on the next poll.
                tracing::warn!(
                    error = %format_args!("{error:#}"),
                    "background mempool mining failed to generate a block"
                );
            }
            Err(BackgroundMineError::Unknown { error, before }) => {
                // Generate reply was lost; the node may have produced a block.
                // Store the pre-generate checkpoint alongside the set and
                // defer reconciliation to the next tick.
                unresolved = Some((before, set));
                tracing::warn!(
                    error = %format_args!("{error:#}"),
                    "background mempool mining lost a generate reply; \
                     deferring reconciliation to next tick"
                );
            }
            Err(BackgroundMineError::Sync(error)) => {
                // A block was produced; record the set to avoid re-mining the
                // same transactions while sync is retried separately.
                attempted = Some(set);
                tracing::warn!(
                    error = %format_args!("{error:#}"),
                    "background mempool mining produced a block but wallet sync failed"
                );
            }
        }
    }
}
