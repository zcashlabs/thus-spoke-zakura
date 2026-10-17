use crate::state::{AppState, notify};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

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
}
