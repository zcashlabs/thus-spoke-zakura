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
use axum::{Json, Router, http::StatusCode, response::IntoResponse};
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

#[test]
fn mining_config_defaults_to_auto() {
    assert_eq!(
        MiningConfig::from_values(None, None).unwrap(),
        MiningConfig::default()
    );
    assert_eq!(
        MiningConfig::from_values(Some(""), None).unwrap().mode,
        MiningMode::Auto
    );
    assert!(
        MiningConfig::from_values(None, None)
            .unwrap()
            .mode
            .auto_mine()
    );
}

#[test]
fn mining_config_decodes_every_mode() {
    let interval = MiningConfig::from_values(Some("interval"), Some("3")).unwrap();
    assert_eq!(interval.mode, MiningMode::Interval);
    assert_eq!(interval.block_time, Some(3));
    assert!(!interval.mode.auto_mine());

    let mempool = MiningConfig::from_values(Some("mempool"), None).unwrap();
    assert_eq!(mempool.mode, MiningMode::Mempool);
    assert_eq!(mempool.block_time, None);
    assert!(!mempool.mode.auto_mine());

    let manual = MiningConfig::from_values(Some("manual"), None).unwrap();
    assert_eq!(manual.mode, MiningMode::Manual);
    assert!(!manual.mode.auto_mine());
}

#[test]
fn mining_config_rejects_inconsistent_values() {
    // Interval mode requires a positive interval.
    assert!(MiningConfig::from_values(Some("interval"), None).is_err());
    assert!(MiningConfig::from_values(Some("interval"), Some("0")).is_err());
    assert!(MiningConfig::from_values(Some("interval"), Some("soon")).is_err());
    // A block time belongs only to interval mode.
    assert!(MiningConfig::from_values(Some("auto"), Some("2")).is_err());
    assert!(MiningConfig::from_values(Some("manual"), Some("2")).is_err());
    // Unknown modes fail loudly rather than silently falling back to auto.
    assert!(MiningConfig::from_values(Some("turbo"), None).is_err());
}

#[tokio::test]
async fn mining_coordinator_reports_an_active_job() {
    let runtime = Arc::new(FakeRuntime::blocked());
    let jobs = Arc::new(MiningCoordinator::new());
    assert!(!jobs.is_active().await);
    let admission = jobs
        .start(runtime.clone(), 1, "active-probe".into())
        .await
        .unwrap();
    let observer = admission.updates.clone();
    assert!(jobs.is_active().await);
    runtime.release(1);
    let update = terminal(observer).await;
    assert_eq!(update.job.state, MiningState::Completed);
    assert!(!jobs.is_active().await);
}

#[test]
fn mempool_mining_skips_an_unchanged_nonempty_set() {
    let mut first = vec!["b".to_owned(), "a".to_owned()];
    let set = new_mempool_set(None, &mut first).expect("a first nonempty set is new");
    assert_eq!(set, vec!["a".to_owned(), "b".to_owned()]);

    // The same transactions, freshly read, must not mine another empty block.
    let mut again = vec!["a".to_owned(), "b".to_owned()];
    assert!(new_mempool_set(Some(set.as_slice()), &mut again).is_none());

    // A changed set is new again.
    let mut changed = vec!["a".to_owned(), "c".to_owned()];
    assert_eq!(
        new_mempool_set(Some(set.as_slice()), &mut changed),
        Some(vec!["a".to_owned(), "c".to_owned()])
    );
}

#[test]
fn mempool_mining_ignores_an_empty_set() {
    let mut empty: Vec<String> = Vec::new();
    assert!(new_mempool_set(None, &mut empty).is_none());
    let previous = vec!["a".to_owned()];
    assert!(new_mempool_set(Some(previous.as_slice()), &mut empty).is_none());
}

// A 64-character hex string used as a stand-in block hash in mock RPCs below.
const FAKE_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
// Genesis hash returned by getblockchaininfo in tests that need a valid
// pre-generate checkpoint but do not exercise chain-tip comparisons.
const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn genesis_checkpoint_response(id: &Value) -> Value {
    json!({
        "result": {
            "chain": "regtest",
            "blocks": 0u64,
            "bestblockhash": GENESIS_HASH,
            "verificationprogress": 1.0
        },
        "error": null,
        "id": id
    })
}

async fn rpc_server(handler: impl Fn(Value) -> Value + Send + Sync + Clone + 'static) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/",
                post(move |Json(req): Json<Value>| {
                    let handler = handler.clone();
                    async move { Json(handler(req)) }
                }),
            ),
        )
        .await
        .unwrap()
    });
    url
}

#[tokio::test]
async fn mine_and_sync_background_returns_generation_error_on_rpc_failure() {
    // An explicit JSON-RPC error from the node proves the request was rejected
    // before execution; generate returning such an error must be Generation.
    let url = rpc_server(|req| match req["method"].as_str() {
        Some("getblockchaininfo") => genesis_checkpoint_response(&req["id"]),
        _ => json!({"result":null,"error":{"code":-1,"message":"no peers"},"id":req["id"]}),
    })
    .await;
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    assert!(matches!(
        state.mine_and_sync_background().await,
        Err(BackgroundMineError::Generation(_))
    ));
}

#[tokio::test]
async fn mine_and_sync_background_returns_sync_error_when_block_fetch_fails() {
    // generate succeeds; getblock fails — a block was produced even though we
    // cannot read its height, so the error must be Sync, not Generation.
    let url = rpc_server(|req| match req["method"].as_str() {
        Some("getblockchaininfo") => genesis_checkpoint_response(&req["id"]),
        Some("generate") => json!({"result":[FAKE_HASH],"error":null,"id":req["id"]}),
        _ => json!({"result":null,"error":{"code":-1,"message":"unavailable"},"id":req["id"]}),
    })
    .await;
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    assert!(matches!(
        state.mine_and_sync_background().await,
        Err(BackgroundMineError::Sync(_))
    ));
}

#[tokio::test]
async fn mine_and_sync_background_returns_sync_error_when_wallet_sync_fails() {
    // generate and getblock both succeed; synchronize_wallet fails because
    // there is no running lightwalletd — the error must still be Sync.
    let url = rpc_server(|req| match req["method"].as_str() {
        Some("getblockchaininfo") => genesis_checkpoint_response(&req["id"]),
        Some("generate") => json!({"result":[FAKE_HASH],"error":null,"id":req["id"]}),
        Some("getblock") => {
            json!({"result":{"height":1,"hash":FAKE_HASH},"error":null,"id":req["id"]})
        }
        _ => json!({"result":null,"error":{"code":-1,"message":"unavailable"},"id":req["id"]}),
    })
    .await;
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    assert!(matches!(
        state.mine_and_sync_background().await,
        Err(BackgroundMineError::Sync(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn mempool_loop_retries_same_set_after_generation_failure() {
    // When generate fails, no block is produced. The transaction set must not
    // be recorded as attempted so it is retried on the very next poll.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let url = rpc_server({
        let tx = tx.clone();
        move |req| match req["method"].as_str() {
            Some("getblockchaininfo") => genesis_checkpoint_response(&req["id"]),
            Some("getrawmempool") => {
                json!({"result":["tx1"],"error":null,"id":req["id"]})
            }
            Some("generate") => {
                let _ = tx.send(());
                json!({"result":null,"error":{"code":-1,"message":"fail"},"id":req["id"]})
            }
            _ => json!({"result":null,"error":{"code":-1,"message":"unexpected"},"id":req["id"]}),
        }
    })
    .await;
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    state.mark_started();
    tokio::spawn(mempool_mining_loop(state));

    // First tick fires at t=0.
    rx.recv().await.expect("first generate call at t=0");
    // Advance past the 2-second interval; generation failure must not have set
    // `attempted`, so the same ["tx1"] is new again and generate is called.
    tokio::time::advance(Duration::from_secs(2)).await;
    rx.recv()
        .await
        .expect("second generate call proves same set is retried after failure");
}

#[tokio::test(start_paused = true)]
async fn mempool_loop_records_attempted_after_block_produced_with_sync_failure() {
    // When generate succeeds but a subsequent step fails (Sync error), the set
    // must be recorded so the same transactions do not trigger a second block.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let url = rpc_server({
        let tx = tx.clone();
        move |req| match req["method"].as_str() {
            Some("getblockchaininfo") => genesis_checkpoint_response(&req["id"]),
            Some("getrawmempool") => {
                json!({"result":["tx1"],"error":null,"id":req["id"]})
            }
            Some("generate") => {
                let _ = tx.send(());
                json!({"result":[FAKE_HASH],"error":null,"id":req["id"]})
            }
            // getblock fails → BackgroundMineError::Sync (block was produced).
            _ => json!({"result":null,"error":{"code":-1,"message":"unavailable"},"id":req["id"]}),
        }
    })
    .await;
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    state.mark_started();
    tokio::spawn(mempool_mining_loop(state));

    // First tick: block produced, sync error, attempted = Some(["tx1"]).
    rx.recv().await.expect("first generate call at t=0");
    // Advance through two more ticks; the unchanged set must be skipped.
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        rx.try_recv().is_err(),
        "generate must not be called again while mempool holds the same set"
    );
}

#[tokio::test]
async fn mine_and_sync_background_returns_unknown_on_malformed_response() {
    // A malformed (non-JSON) body from generate cannot confirm no block was
    // produced; the error must be Unknown, not Generation.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/",
                post(|Json(req): Json<Value>| async move {
                    match req["method"].as_str() {
                        Some("getblockchaininfo") => {
                            Json(genesis_checkpoint_response(&req["id"])).into_response()
                        }
                        _ => (StatusCode::OK, "not-a-valid-rpc-response").into_response(),
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    assert!(matches!(
        state.mine_and_sync_background().await,
        Err(BackgroundMineError::Unknown { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn mempool_loop_suppresses_retry_after_lost_generate_reply() {
    // When the generate reply is lost (transport/decode failure), the node may
    // have already produced a block. The loop must reconcile against the chain
    // tip and suppress re-mining the same transaction set.
    //
    // The pre-generate checkpoint returns the genesis tip; the reconciliation
    // checkpoint returns a new tip — confirming the block was produced.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let checkpoint_calls = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn({
        let tx = tx.clone();
        async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/",
                    post(move |Json(req): Json<Value>| {
                        let tx = tx.clone();
                        let checkpoint_calls = checkpoint_calls.clone();
                        async move {
                            match req["method"].as_str() {
                                Some("getrawmempool") => {
                                    Json(json!({"result":["tx1"],"error":null,"id":req["id"]}))
                                        .into_response()
                                }
                                Some("generate") => {
                                    // Signal the test, then return a malformed body.
                                    // The block was produced but the reply is lost.
                                    let _ = tx.send(());
                                    (StatusCode::OK, "not-a-valid-rpc-response").into_response()
                                }
                                Some("getblockchaininfo") => {
                                    let n = checkpoint_calls.fetch_add(1, Ordering::SeqCst);
                                    let (blocks, hash) = if n == 0 {
                                        // Before generate: chain at genesis.
                                        (0u64, "0000000000000000000000000000000000000000000000000000000000000000")
                                    } else {
                                        // Reconciliation: chain has advanced.
                                        (1u64, FAKE_HASH)
                                    };
                                    Json(json!({
                                        "result": {
                                            "chain": "regtest",
                                            "blocks": blocks,
                                            "bestblockhash": hash,
                                            "verificationprogress": 1.0
                                        },
                                        "error": null,
                                        "id": req["id"]
                                    }))
                                    .into_response()
                                }
                                _ => Json(json!({"result":null,"error":{"code":-1,"message":"unexpected"},"id":req["id"]})).into_response(),
                            }
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        }
    });

    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    state.mark_started();
    tokio::spawn(mempool_mining_loop(state));

    // First tick fires at t=0: before=genesis, generate → Unknown →
    // unresolved = {genesis, ["tx1"]}.
    rx.recv().await.expect("first generate call at t=0");

    // Advance to t=2: reconciliation tick reads current tip (H1), which differs
    // from before (genesis) → chain advanced → attempted = Some(["tx1"]).
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    // Advance through two more ticks; the unchanged set must be skipped.
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        rx.try_recv().is_err(),
        "generate must not be called again after a lost reply is reconciled as produced"
    );
}

#[tokio::test]
async fn mine_and_sync_background_returns_unknown_on_gateway_timeout() {
    // HTTP 504 can occur after the node has forwarded and executed the request;
    // it must not be treated as a pre-execution rejection.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/",
                post(|Json(req): Json<Value>| async move {
                    match req["method"].as_str() {
                        Some("getblockchaininfo") => {
                            Json(genesis_checkpoint_response(&req["id"])).into_response()
                        }
                        _ => (StatusCode::GATEWAY_TIMEOUT, "").into_response(),
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    assert!(
        matches!(
            state.mine_and_sync_background().await,
            Err(BackgroundMineError::Unknown { .. })
        ),
        "HTTP 504 must be Unknown, not Generation"
    );
}

#[tokio::test]
async fn mine_and_sync_background_returns_unknown_on_empty_hash_result() {
    // A success response carrying no hash does not prove no block was produced;
    // the node may have generated before returning the unusable acknowledgment.
    let url = rpc_server(|req| match req["method"].as_str() {
        Some("getblockchaininfo") => genesis_checkpoint_response(&req["id"]),
        Some("generate") => json!({"result":[],"error":null,"id":req["id"]}),
        _ => json!({"result":null,"error":{"code":-1,"message":"unexpected"},"id":req["id"]}),
    })
    .await;
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    assert!(
        matches!(
            state.mine_and_sync_background().await,
            Err(BackgroundMineError::Unknown { .. })
        ),
        "empty hash result must be Unknown, not Generation"
    );
}

#[tokio::test(start_paused = true)]
async fn mempool_loop_retries_when_generate_fails_without_producing_block() {
    // When the generate reply is lost but the chain tip did not advance,
    // no block was produced and the transaction set must be retried.
    // This covers both "first attempt before any block exists" (before = H0,
    // current = H0) and "unrelated block present before generate" (stale
    // last_checkpoint from a prior mine would have caused false suppression
    // in the old design; the fresh before-generate snapshot avoids it).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/",
                post(move |Json(req): Json<Value>| {
                    let tx = tx.clone();
                    async move {
                        match req["method"].as_str() {
                            Some("getrawmempool") => {
                                Json(json!({"result":["tx1"],"error":null,"id":req["id"]}))
                                    .into_response()
                            }
                            Some("generate") => {
                                let _ = tx.send(());
                                // Reply is lost; before == current after this,
                                // so reconciliation must allow a retry.
                                (StatusCode::OK, "not-a-valid-rpc-response").into_response()
                            }
                            Some("getblockchaininfo") => {
                                // Tip never changes: our generate produced no block.
                                Json(json!({
                                    "result": {
                                        "chain": "regtest",
                                        "blocks": 0,
                                        "bestblockhash": "0000000000000000000000000000000000000000000000000000000000000000",
                                        "verificationprogress": 1.0
                                    },
                                    "error": null,
                                    "id": req["id"]
                                }))
                                .into_response()
                            }
                            _ => Json(json!({"result":null,"error":{"code":-1,"message":"unexpected"},"id":req["id"]})).into_response(),
                        }
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });

    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    state.mark_started();
    tokio::spawn(mempool_mining_loop(state));

    // Tick 0: before=H0, generate → Unknown → unresolved = {H0, ["tx1"]}.
    rx.recv().await.expect("first generate call at t=0");

    // Tick 2 (t=2): reconcile — current=H0 == before(H0) → no block → retry.
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    // Tick 4 (t=4): mining tick — same set is new again → second generate.
    tokio::time::advance(Duration::from_secs(2)).await;
    rx.recv().await.expect(
        "second generate call proves the set is retried after reconciliation shows no block",
    );
}

#[tokio::test(start_paused = true)]
async fn mempool_loop_defers_reconciliation_when_checkpoint_temporarily_unavailable() {
    // When the checkpoint read fails during reconciliation, the loop must carry
    // the unresolved attempt forward rather than permanently suppressing the set.
    // Once the checkpoint becomes available again, reconciliation proceeds.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let checkpoint_calls = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn({
        async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/",
                    post(move |Json(req): Json<Value>| {
                        let tx = tx.clone();
                        let checkpoint_calls = checkpoint_calls.clone();
                        async move {
                            match req["method"].as_str() {
                                Some("getrawmempool") => {
                                    Json(json!({"result":["tx1"],"error":null,"id":req["id"]}))
                                        .into_response()
                                }
                                Some("generate") => {
                                    let _ = tx.send(());
                                    (StatusCode::OK, "not-a-valid-rpc-response").into_response()
                                }
                                Some("getblockchaininfo") => {
                                    let n = checkpoint_calls.fetch_add(1, Ordering::SeqCst);
                                    if n == 1 {
                                        // First reconciliation attempt: temporarily unavailable.
                                        Json(json!({"result":null,"error":{"code":-1,"message":"temporarily unavailable"},"id":req["id"]})).into_response()
                                    } else {
                                        // Before-generate and subsequent: stable genesis tip.
                                        Json(json!({
                                            "result": {
                                                "chain": "regtest",
                                                "blocks": 0,
                                                "bestblockhash": "0000000000000000000000000000000000000000000000000000000000000000",
                                                "verificationprogress": 1.0
                                            },
                                            "error": null,
                                            "id": req["id"]
                                        })).into_response()
                                    }
                                }
                                _ => Json(json!({"result":null,"error":{"code":-1,"message":"unexpected"},"id":req["id"]})).into_response(),
                            }
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        }
    });

    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    state.mark_started();
    tokio::spawn(mempool_mining_loop(state));

    // Tick 0: before=H0, generate → Unknown → unresolved = {H0, ["tx1"]}.
    rx.recv().await.expect("first generate call at t=0");

    // Tick 2 (t=2): reconcile — checkpoint fails → defer (unresolved retained).
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    // Tick 4 (t=4): reconcile — checkpoint returns H0 == before(H0) → no block → retry.
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    // Tick 6 (t=6): mining tick → second generate call confirms the set was retried.
    tokio::time::advance(Duration::from_secs(2)).await;
    rx.recv()
        .await
        .expect("second generate call proves the set is retried after deferred reconciliation");
}

#[tokio::test]
async fn mine_and_sync_background_returns_unknown_on_bad_gateway() {
    // HTTP 502 can come after a proxy forwarded the request and failed to read
    // the upstream response; it must not be treated as a pre-execution rejection.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/",
                post(|Json(req): Json<Value>| async move {
                    match req["method"].as_str() {
                        Some("getblockchaininfo") => {
                            Json(genesis_checkpoint_response(&req["id"])).into_response()
                        }
                        _ => (StatusCode::BAD_GATEWAY, "").into_response(),
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    assert!(
        matches!(
            state.mine_and_sync_background().await,
            Err(BackgroundMineError::Unknown { .. })
        ),
        "HTTP 502 must be Unknown, not Generation"
    );
}

#[tokio::test(start_paused = true)]
async fn mempool_loop_skips_and_retries_when_pre_generate_checkpoint_fails() {
    // When the pre-generate checkpoint read fails, mine_and_sync_background
    // returns Generation without calling generate. The loop treats Generation as
    // retry-safe: the transaction set is not recorded as attempted and generate
    // fires normally on the very next tick after the checkpoint is restored.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let checkpoint_calls = Arc::new(AtomicUsize::new(0));
    let url = rpc_server({
        let tx = tx.clone();
        let checkpoint_calls = checkpoint_calls.clone();
        move |req| match req["method"].as_str() {
            Some("getrawmempool") => json!({"result":["tx1"],"error":null,"id":req["id"]}),
            Some("generate") => {
                let _ = tx.send(());
                json!({"result":null,"error":{"code":-1,"message":"fail"},"id":req["id"]})
            }
            Some("getblockchaininfo") => {
                let n = checkpoint_calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    // Tick 0: checkpoint unavailable → Generation, generate skipped.
                    json!({"result":null,"error":{"code":-1,"message":"unavailable"},"id":req["id"]})
                } else {
                    genesis_checkpoint_response(&req["id"])
                }
            }
            _ => json!({"result":null,"error":{"code":-1,"message":"unexpected"},"id":req["id"]}),
        }
    })
    .await;

    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    state.mark_started();
    tokio::spawn(mempool_mining_loop(state));

    // Tick 0: checkpoint fails → Generation → generate NOT called, set not recorded.
    // Advance to tick 2: checkpoint succeeds → generate fires.
    tokio::time::advance(Duration::from_secs(2)).await;
    rx.recv()
        .await
        .expect("generate fires on the tick after checkpoint failure");
}

#[tokio::test(start_paused = true)]
async fn interval_loop_waits_for_chain_operation_before_generating() {
    // interval_mining_loop must not mine while a chain operation (e.g. a reorg)
    // holds the chain_operations write lock.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let url = rpc_server({
        let tx = tx.clone();
        move |req| match req["method"].as_str() {
            Some("getblockchaininfo") => genesis_checkpoint_response(&req["id"]),
            Some("generate") => {
                let _ = tx.send(());
                json!({"result":null,"error":{"code":-1,"message":"fail"},"id":req["id"]})
            }
            _ => json!({"result":null,"error":{"code":-1,"message":"unexpected"},"id":req["id"]}),
        }
    })
    .await;
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    state.mark_started();

    // Clone the lock handle before moving state so we can hold the write lock
    // without borrowing from the moved value.
    let chain_ops = Arc::clone(&state.0.mining.chain_operations);
    let chain = chain_ops.write().await;
    tokio::spawn(interval_mining_loop(state, Duration::from_secs(2)));

    // Advance past the interval; generate must not fire while lock is held.
    tokio::time::advance(Duration::from_secs(3)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        rx.try_recv().is_err(),
        "generate must not fire while chain operation holds the lock"
    );

    // Release the lock; the next interval tick must call generate.
    drop(chain);
    tokio::time::advance(Duration::from_secs(2)).await;
    rx.recv()
        .await
        .expect("generate must fire after the chain operation lock is released");
}

#[tokio::test(start_paused = true)]
async fn mempool_loop_waits_for_chain_operation_before_generating() {
    // mempool_mining_loop must not mine while a chain operation holds the
    // chain_operations write lock.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let url = rpc_server({
        let tx = tx.clone();
        move |req| match req["method"].as_str() {
            Some("getblockchaininfo") => genesis_checkpoint_response(&req["id"]),
            Some("getrawmempool") => {
                json!({"result":["tx1"],"error":null,"id":req["id"]})
            }
            Some("generate") => {
                let _ = tx.send(());
                json!({"result":null,"error":{"code":-1,"message":"fail"},"id":req["id"]})
            }
            _ => json!({"result":null,"error":{"code":-1,"message":"unexpected"},"id":req["id"]}),
        }
    })
    .await;
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    state.mark_started();

    let chain_ops = Arc::clone(&state.0.mining.chain_operations);
    let chain = chain_ops.write().await;
    tokio::spawn(mempool_mining_loop(state));

    // Advance past the mempool poll interval; generate must not fire.
    tokio::time::advance(Duration::from_secs(3)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        rx.try_recv().is_err(),
        "generate must not fire while chain operation holds the lock"
    );

    // Release the lock; the next tick must call generate.
    drop(chain);
    tokio::time::advance(Duration::from_secs(2)).await;
    rx.recv()
        .await
        .expect("generate must fire after the chain operation lock is released");
}

#[tokio::test]
async fn mine_and_sync_background_skips_generation_when_checkpoint_fails() {
    // When the pre-generate checkpoint read fails, mine_and_sync_background must
    // return Generation without calling generate. Without a valid baseline the
    // outcome of a lost reply cannot be reconciled, so it is safer to skip.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let url = rpc_server({
        let tx = tx.clone();
        move |req| match req["method"].as_str() {
            Some("generate") => {
                // Should never be reached.
                let _ = tx.send(());
                json!({"result":[FAKE_HASH],"error":null,"id":req["id"]})
            }
            // getblockchaininfo (and everything else) returns an RPC error so
            // the checkpoint call fails.
            _ => json!({"result":null,"error":{"code":-1,"message":"unavailable"},"id":req["id"]}),
        }
    })
    .await;
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    assert!(matches!(
        state.mine_and_sync_background().await,
        Err(BackgroundMineError::Generation(_))
    ));
    assert!(
        rx.try_recv().is_err(),
        "generate must not be called when the pre-generate checkpoint fails"
    );
}

#[tokio::test]
async fn mine_and_sync_background_returns_unknown_on_service_unavailable() {
    // HTTP 503 (Service Unavailable) can be returned by Envoy after it resets an
    // upstream connection; the request may have been forwarded and executed. It
    // must not be treated as a pre-execution rejection.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/",
                post(|Json(req): Json<Value>| async move {
                    match req["method"].as_str() {
                        Some("getblockchaininfo") => {
                            Json(genesis_checkpoint_response(&req["id"])).into_response()
                        }
                        _ => (StatusCode::SERVICE_UNAVAILABLE, "").into_response(),
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    assert!(
        matches!(
            state.mine_and_sync_background().await,
            Err(BackgroundMineError::Unknown { .. })
        ),
        "HTTP 503 must be Unknown, not Generation"
    );
}
