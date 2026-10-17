use super::*;
use crate::api::mining::tests::mining_request;
use axum::routing::post;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;

struct FakeRuntime {
    permits: Semaphore,
    sync_permits: Semaphore,
    calls: AtomicU32,
    scans: AtomicU32,
    fail_call: u32,
    panic_call: u32,
    fail_sync: bool,
}

impl FakeRuntime {
    fn blocked() -> Self {
        Self {
            permits: Semaphore::new(0),
            sync_permits: Semaphore::new(1),
            calls: AtomicU32::new(0),
            scans: AtomicU32::new(0),
            fail_call: 0,
            panic_call: 0,
            fail_sync: false,
        }
    }

    fn release(&self, count: u32) {
        self.permits.add_permits(count as usize);
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl MiningRuntime for FakeRuntime {
    async fn generate_one(&self) -> anyhow::Result<String> {
        self.permits.acquire().await.unwrap().forget();
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        assert_ne!(call, self.panic_call, "injected worker panic");
        anyhow::ensure!(call != self.fail_call, "injected RPC failure");
        Ok(format!("{call:064x}"))
    }

    async fn synchronize(&self, tip_hash: &str) -> anyhow::Result<()> {
        assert_eq!(tip_hash.len(), 64);
        self.scans.fetch_add(1, Ordering::SeqCst);
        self.sync_permits.acquire().await.unwrap().forget();
        anyhow::ensure!(!self.fail_sync, "injected sync failure");
        Ok(())
    }

    fn notify(&self, _topic: &'static str) {}
}

#[tokio::test]
async fn chain_operation_excludes_new_mining_but_preserves_job_replays() {
    let runtime = Arc::new(FakeRuntime::blocked());
    let jobs = Arc::new(MiningCoordinator::new());
    runtime.release(1);
    let accepted = jobs
        .start(runtime.clone(), 1, "before-reorg".into())
        .await
        .unwrap();
    let id = accepted.job.id.clone();
    terminal(accepted.updates).await;
    let chain = jobs.chain_operations.write().await;
    assert_eq!(
        jobs.start(runtime.clone(), 1, "before-reorg".into())
            .await
            .unwrap()
            .job
            .id,
        id
    );
    assert!(matches!(
        jobs.start(runtime.clone(), 1, "during-reorg".into()).await,
        Err(MiningAdmissionError::ChainBusy)
    ));
    assert_eq!(runtime.calls(), 1);
    drop(chain);
    runtime.release(1);
    runtime.sync_permits.add_permits(1);
    let next = jobs
        .start(runtime.clone(), 1, "after-reorg".into())
        .await
        .unwrap();
    terminal(next.updates).await;
    assert_eq!(runtime.calls(), 2);
}

#[tokio::test]
async fn mining_holds_chain_exclusion_through_wallet_synchronization() {
    let runtime = Arc::new(FakeRuntime::blocked());
    runtime.sync_permits.acquire().await.unwrap().forget();
    runtime.release(1);
    let jobs = Arc::new(MiningCoordinator::new());
    let accepted = jobs
        .start(runtime.clone(), 1, "sync-exclusion".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.scans.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(jobs.chain_operations.try_write().is_err());
    runtime.sync_permits.add_permits(1);
    terminal(accepted.updates).await;
    let _chain = jobs.chain_operations.write().await;
}

async fn terminal(mut updates: tokio::sync::watch::Receiver<MiningUpdate>) -> MiningUpdate {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let update = updates.borrow_and_update().clone();
            if matches!(
                update.job.state,
                MiningState::Completed | MiningState::Failed
            ) {
                return update;
            }
            updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn mining_continues_without_request_receiver() {
    let runtime = Arc::new(FakeRuntime::blocked());
    let jobs = Arc::new(MiningCoordinator::new());
    let admission = jobs
        .start(runtime.clone(), 3, "disconnect-test".into())
        .await
        .unwrap();
    let id = admission.job.id;
    let observer = admission.updates.clone();
    drop(admission.updates);
    runtime.release(3);
    let update = terminal(observer).await;
    assert_eq!(update.job.state, MiningState::Completed);
    assert_eq!(update.hashes.unwrap().len(), 3);
    let persisted = jobs.get(&id).await.unwrap();
    assert_eq!(persisted.completed_blocks, 3);
    assert_eq!(persisted.hashes.unwrap().len(), 3);
    assert_eq!(runtime.calls(), 3);
    assert_eq!(runtime.scans.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mining_continues_with_zero_receivers() {
    let runtime = Arc::new(FakeRuntime::blocked());
    let jobs = Arc::new(MiningCoordinator::new());
    let admission = jobs
        .start(runtime.clone(), 2, "zero-receivers".into())
        .await
        .unwrap();
    drop(admission.updates);
    runtime.release(2);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let replay = jobs
                .start(runtime.clone(), 2, "zero-receivers".into())
                .await
                .unwrap();
            if replay.job.state == MiningState::Completed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(runtime.calls(), 2);
}

#[tokio::test]
async fn concurrent_replay_has_one_worker_and_conflicts_are_rejected() {
    let runtime = Arc::new(FakeRuntime::blocked());
    let jobs = Arc::new(MiningCoordinator::new());
    let (first, replay) = tokio::join!(
        jobs.start(runtime.clone(), 2, "same-key".into()),
        jobs.start(runtime.clone(), 2, "same-key".into()),
    );
    let first = first.unwrap();
    assert_eq!(first.job.id, replay.unwrap().job.id);
    assert!(matches!(
        jobs.start(runtime.clone(), 3, "same-key".into()).await,
        Err(MiningAdmissionError::KeyConflict)
    ));
    assert!(matches!(
        jobs.start(runtime.clone(), 2, "other-key".into()).await,
        Err(MiningAdmissionError::Busy)
    ));
    runtime.release(2);
    terminal(first.updates).await;
    assert_eq!(runtime.calls(), 2);
}

#[tokio::test]
async fn validates_blocks_and_keys() {
    let runtime = Arc::new(FakeRuntime::blocked());
    let jobs = Arc::new(MiningCoordinator::new());
    for blocks in [0, 10_001] {
        assert!(matches!(
            jobs.start(runtime.clone(), blocks, "valid-key".into())
                .await,
            Err(MiningAdmissionError::InvalidBlocks)
        ));
    }
    for key in [
        "short".into(),
        "x".repeat(129),
        "space key".into(),
        "nonasciié".into(),
        "control\n".into(),
    ] {
        assert!(matches!(
            jobs.start(runtime.clone(), 1, key).await,
            Err(MiningAdmissionError::InvalidKey)
        ));
    }
    assert!(jobs.latest().await.is_none());
}

#[tokio::test]
async fn capacity_keeps_old_replays_and_history_without_eviction() {
    let runtime = Arc::new(FakeRuntime::blocked());
    let jobs = Arc::new(MiningCoordinator::with_capacity(2));
    let first = jobs
        .start(runtime.clone(), 1, "first-key".into())
        .await
        .unwrap();
    let first_id = first.job.id;
    runtime.release(1);
    terminal(first.updates).await;
    runtime.sync_permits.add_permits(1);
    let second = jobs
        .start(runtime.clone(), 1, "second-key".into())
        .await
        .unwrap();
    let second_id = second.job.id;
    runtime.release(1);
    terminal(second.updates).await;
    assert!(matches!(
        jobs.start(runtime.clone(), 1, "third-key".into()).await,
        Err(MiningAdmissionError::Capacity)
    ));
    let replay = jobs
        .start(runtime.clone(), 1, "first-key".into())
        .await
        .unwrap();
    assert_eq!(replay.job.id, first_id);
    assert_eq!(replay.updates.borrow().job.id, first_id);
    assert!(replay.updates.borrow().hashes.is_none());
    assert_eq!(jobs.latest().await.unwrap().id, second_id);
    assert!(jobs.get(&first_id).await.is_some());
    assert!(jobs.get("unknown").await.is_none());
    assert_eq!(runtime.calls(), 2);
}

#[tokio::test]
async fn rpc_failure_preserves_acknowledged_count_without_retry() {
    let runtime = Arc::new(FakeRuntime {
        fail_call: 2,
        ..FakeRuntime::blocked()
    });
    let jobs = Arc::new(MiningCoordinator::new());
    let first = jobs
        .start(runtime.clone(), 3, "failure-key".into())
        .await
        .unwrap();
    let id = first.job.id;
    runtime.release(3);
    let failed = terminal(first.updates).await.job;
    assert_eq!(failed.state, MiningState::Failed);
    assert_eq!(failed.completed_blocks, 1);
    assert!(failed.progress_uncertain);
    assert!(failed.error.unwrap().contains("injected RPC failure"));
    let replay = jobs
        .start(runtime.clone(), 3, "failure-key".into())
        .await
        .unwrap();
    assert_eq!(replay.job.id, id);
    assert_eq!(replay.job.state, MiningState::Failed);
    assert_eq!(runtime.calls(), 2);
    assert_eq!(runtime.scans.load(Ordering::SeqCst), 0);
    runtime.sync_permits.add_permits(1);
    let next = jobs
        .start(runtime.clone(), 1, "next-key".into())
        .await
        .unwrap();
    terminal(next.updates).await;
    assert_eq!(runtime.calls(), 3);
}

#[tokio::test]
async fn synchronization_failure_has_full_known_count() {
    let runtime = Arc::new(FakeRuntime {
        fail_sync: true,
        ..FakeRuntime::blocked()
    });
    let jobs = Arc::new(MiningCoordinator::new());
    let admission = jobs
        .start(runtime.clone(), 2, "sync-failure".into())
        .await
        .unwrap();
    runtime.release(2);
    let failed = terminal(admission.updates).await.job;
    assert_eq!(failed.state, MiningState::Failed);
    assert_eq!(failed.completed_blocks, 2);
    assert!(!failed.progress_uncertain);
    assert!(failed.hashes.is_none());
    assert!(
        failed
            .error
            .unwrap()
            .starts_with("Blocks mined, but wallet synchronization failed")
    );
    assert_eq!(runtime.calls(), 2);
}

#[tokio::test]
async fn syncing_keeps_admission_busy() {
    let runtime = Arc::new(FakeRuntime {
        sync_permits: Semaphore::new(0),
        ..FakeRuntime::blocked()
    });
    let jobs = Arc::new(MiningCoordinator::new());
    let mut admission = jobs
        .start(runtime.clone(), 1, "syncing-key".into())
        .await
        .unwrap();
    runtime.release(1);
    tokio::time::timeout(Duration::from_secs(2), async {
        while admission.updates.borrow_and_update().job.state != MiningState::Syncing {
            admission.updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        jobs.start(runtime.clone(), 1, "other-key".into()).await,
        Err(MiningAdmissionError::Busy)
    ));
    runtime.sync_permits.add_permits(1);
    terminal(admission.updates).await;
}

#[tokio::test]
async fn worker_panic_publishes_failure_and_releases_admission() {
    let runtime = Arc::new(FakeRuntime {
        panic_call: 2,
        ..FakeRuntime::blocked()
    });
    let jobs = Arc::new(MiningCoordinator::new());
    let admission = jobs
        .start(runtime.clone(), 3, "panic-key".into())
        .await
        .unwrap();
    runtime.release(3);
    let failed = terminal(admission.updates).await.job;
    assert_eq!(failed.state, MiningState::Failed);
    assert_eq!(failed.completed_blocks, 1);
    assert!(failed.progress_uncertain);
    assert!(failed.error.unwrap().contains("worker"));
    let next = jobs
        .start(runtime.clone(), 1, "after-panic".into())
        .await
        .unwrap();
    assert_eq!(
        terminal(next.updates).await.job.state,
        MiningState::Completed
    );
    assert_eq!(runtime.calls(), 3);
}

use crate::api::router;
use crate::rpc::NodeRpc;
use crate::state::tests::state_with_local_wallet;
use axum::{Json, Router, http::StatusCode};
use serde_json::{Value, json};
use std::sync::atomic::AtomicUsize;
#[tokio::test]
async fn mining_adapter_rejects_missing_empty_and_multiple_rpc_hashes() {
    for hashes in [json!([]), json!([""]), json!(["hash-one", "hash-two"])] {
        let calls = Arc::new(AtomicUsize::new(0));
        let rpc_calls = calls.clone();
        let rpc = Router::new().route(
            "/",
            post(move |Json(request): Json<Value>| {
                let hashes = hashes.clone();
                let calls = rpc_calls.clone();
                async move {
                    assert_eq!(request["method"], "generate");
                    assert_eq!(request["params"], json!([1]));
                    calls.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"jsonrpc":"2.0","id":request["id"],"result":hashes,"error":null}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, rpc).await.unwrap();
        });
        let (mut state, _dir) = state_with_local_wallet();
        Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(endpoint);
        state.mark_started();
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
