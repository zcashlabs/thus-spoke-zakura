use crate::api::router;
use crate::mining::{MiningCoordinator, MiningRuntime};
use crate::state::AppState;
use crate::state::tests::started_state_with_local_wallet as state_with_local_wallet;
use axum::body::Body;
use axum::{
    Router,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tower::ServiceExt;
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

pub(crate) async fn mining_request(
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
    let (status, first) = mining_request(&app, "POST", "/api/v1/mining/jobs", body.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(first["requested_blocks"], 3);
    assert_eq!(first["completed_blocks"], 0);
    assert_eq!(first["state"], "mining");
    assert_eq!(first["error"], Value::Null);
    assert_eq!(first["progress_uncertain"], false);
    assert_eq!(first["hashes"], Value::Null);
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
        let (status, error) = mining_request(&app, "POST", "/api/v1/mining/jobs", conflict).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error["error"]["status"], 409);
    }
    let (status, error) = mining_request(&app, "POST", "/api/v1/mine", json!({"blocks":1})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["status"], 409);
    let (status, error) =
        mining_request(&app, "GET", "/api/v1/mining/jobs/unknown", Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error["error"]["status"], 404);
    runtime.release(3);
    let completed = completed_mining(&app).await;
    assert_eq!(completed["completed_blocks"], 3);
    assert_eq!(completed["hashes"].as_array().unwrap().len(), 3);
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
    let (status, error) = mining_request(&app, "POST", "/api/v1/mine", json!({"blocks":1})).await;
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
            let (_, value) = mining_request(&app, "GET", "/api/v1/mining/jobs", Value::Null).await;
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
