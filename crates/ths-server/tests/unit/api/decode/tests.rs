use crate::api::router;
use crate::state::{AppState, tests::state_with_local_wallet};
use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;

const V6_TRANSACTION: &str = "0600008098b684d85b16a53701000000020000000100000000000000000000000000000000000000000000000000000000000000000000000000ffffffff0150c3000000000000015100000000";

async fn post_decode(state: AppState, hex: &str) -> (StatusCode, Value) {
    let response = router(state)
        .oneshot(
            Request::post("/api/v1/transactions/decode")
                .header("content-type", "application/json")
                .body(Body::from(json!({"hex": hex}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn decodes_unbroadcast_transaction_without_touching_wallet_or_node() {
    let (local, _dir) = state_with_local_wallet();
    let before = local.0.store.account(1).unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node = format!("http://{}", listener.local_addr().unwrap());
    let rpc = Router::new().route(
        "/",
        post(move || {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                Json(json!({"result": null, "error": null, "id": 1}))
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, rpc).await.unwrap() });
    let state = AppState::new(
        local.0.store.clone(),
        local.0.wallet.clone(),
        node,
        "test".into(),
    );

    let (status, decoded) = post_decode(state.clone(), V6_TRANSACTION).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        decoded["txid"],
        "7931e7795fc1b0a793331c2b9a3eed8d86c671b3a3eac49f19ea9a7d88161ee1"
    );
    assert_eq!(decoded["version"], 6);
    assert_eq!(decoded["branch_id"], "0x37a5165b");
    assert_eq!(decoded["expiry_height"], 2);
    assert_eq!(
        decoded["transparent_inputs"],
        json!([{"txid": "0000000000000000000000000000000000000000000000000000000000000000", "vout": 0}])
    );
    assert_eq!(
        decoded["transparent_outputs"],
        json!([{"value_zatoshi": 50_000, "script_pubkey": "51"}])
    );
    assert_eq!(
        decoded["shielded_action_counts"],
        json!({"sprout_joinsplits": 0, "sapling_spends": 0, "sapling_outputs": 0, "orchard_actions": 0, "ironwood_actions": 0})
    );
    assert_eq!(
        decoded["public_value_balances"],
        json!({"sprout_zatoshi": 0, "sapling_zatoshi": 0, "orchard_zatoshi": 0, "ironwood_zatoshi": 0})
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    let after = state.0.store.account(1).unwrap();
    assert_eq!(before, after);
}

#[tokio::test]
async fn malformed_and_truncated_transactions_return_json_400() {
    let (state, _dir) = state_with_local_wallet();
    for (raw, expected) in [
        ("not-hex".to_owned(), "invalid hex"),
        ("06000080".to_owned(), "invalid transaction"),
        (format!("{V6_TRANSACTION}00"), "trailing bytes"),
    ] {
        let (status, error) = post_decode(state.clone(), &raw).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}");
        assert_eq!(error["error"]["status"], 400, "{raw}");
        assert!(
            error["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(expected)),
            "{raw}"
        );
    }
}

#[tokio::test]
async fn oversized_transaction_returns_json_413() {
    let (state, _dir) = state_with_local_wallet();
    // hex for a 2 MB transaction is over the default body limit but reaches the decoder
    let (status, _) = post_decode(state.clone(), &"00".repeat(2_000_000)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let raw = "00".repeat(2 * 1024 * 1024);
    let (status, error) = post_decode(state, &raw).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error["error"]["status"], 413);
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("maximum allowed size"))
    );
}
