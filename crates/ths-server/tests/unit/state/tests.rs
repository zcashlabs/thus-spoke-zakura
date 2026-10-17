use crate::api::router;
use crate::mining::{MiningConfig, MiningMode};
use crate::{db::Store, state::AppState, wallet::RealWallet};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
pub(crate) fn state_with_local_wallet() -> (AppState, tempfile::TempDir) {
    state_with_mining(MiningConfig::default())
}

pub(crate) fn state_with_mining(mining: MiningConfig) -> (AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(dir.path().join("server.db")).expect("open store");
    store.initialize().expect("initialize store");
    let wallet =
        RealWallet::open(dir.path(), &store.seed().expect("wallet seed")).expect("open wallet");
    (
        AppState::new_with_mining(
            store,
            wallet,
            "http://127.0.0.1:1".into(),
            "test".into(),
            mining,
        ),
        dir,
    )
}

pub(crate) fn state_with_mode(mode: MiningMode) -> (AppState, tempfile::TempDir) {
    state_with_mining(MiningConfig {
        mode,
        block_time: matches!(mode, MiningMode::Interval).then_some(2),
    })
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

pub(crate) fn offline_state() -> (AppState, tempfile::TempDir) {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    let state = AppState::new(store, wallet, "http://127.0.0.1:1".into(), "test".into());
    (state, dir)
}

use axum::{Json, Router, routing::post};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub(crate) async fn api_request(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(if method == "GET" {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
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

pub(crate) async fn api_get(state: &AppState, uri: &str) -> (StatusCode, Value) {
    api_request(state, "GET", uri, Value::Null).await
}

pub(crate) async fn counting_rpc(chain_info: bool) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let rpc_calls = calls.clone();
    let rpc = Router::new().route("/", post(move |Json(request): Json<Value>| {
        let calls = rpc_calls.clone();
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Json(if chain_info && request["method"] == "getblockchaininfo" {
                json!({"result": {"chain": "regtest", "blocks": 101, "bestblockhash": "a".repeat(64)}, "error": null, "id": request["id"]})
            } else {
                json!({"result": null, "error": {"code": -1, "message": "unavailable"}, "id": request["id"]})
            })
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, rpc).await.unwrap() });
    (url, calls)
}

pub(crate) fn started_state_with_local_wallet() -> (AppState, tempfile::TempDir) {
    let (state, dir) = state_with_local_wallet();
    state.mark_started();
    (state, dir)
}

pub(crate) fn started_offline_state() -> (AppState, tempfile::TempDir) {
    let (state, dir) = offline_state();
    state.mark_started();
    (state, dir)
}
