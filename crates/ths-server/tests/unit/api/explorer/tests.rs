use super::*;
use crate::api::router;
use crate::state::AppState;
use crate::state::tests::state_with_local_wallet;
use axum::body::Body;
use axum::routing::post;
use axum::{
    Json, Router,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const EMPTY_SEARCH: &str =
    "q must be a block height, block hash, transaction id, or transparent address";
const UNTRIMMED_ADDRESS: &str = "address must not have leading or trailing whitespace";

async fn node_that_records_calls() -> (String, Arc<Mutex<Vec<String>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/",
        post(move |Json(req): Json<Value>| {
            let recorded = recorded.clone();
            async move {
                recorded
                    .lock()
                    .unwrap()
                    .push(req["method"].as_str().unwrap().to_owned());
                Json(json!({"result": {"balance": 0}, "error": null, "id": req["id"]}))
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await });
    (node, calls)
}

fn state_at(node: String) -> (AppState, tempfile::TempDir) {
    let (state, dir) = state_with_local_wallet();
    let state = AppState::new(
        state.0.store.clone(),
        state.0.wallet.clone(),
        node,
        "test".into(),
    );
    (state, dir)
}

async fn assert_bad_request(state: AppState, uri: &str, message: &str) {
    let response = router(state)
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"error": {"message": message, "status": 400}}),
        "{uri}"
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
    assert!(attach_transparent_prevouts(&mut tx, &prev));
    assert_eq!(tx["vin"][0]["valueZat"], 50_000_000);
    assert_eq!(tx["vin"][0]["scriptPubKey"]["addresses"][0], "tmABC");
}

#[test]
fn leaves_coinbase_inputs_untouched() {
    let mut tx = json!({"vin": [{"coinbase": "00"}]});
    assert!(attach_transparent_prevouts(&mut tx, &Default::default()));
    assert_eq!(tx["vin"][0]["coinbase"], "00");
    assert!(tx["vin"][0].get("valueZat").is_none());
}

#[test]
fn marks_only_inputs_whose_previous_output_is_unavailable() {
    let mut tx = json!({"vin": [
        {"txid": "aa", "vout": 0},
        {"txid": "bb", "vout": 1}
    ]});
    let prev = std::collections::HashMap::from([(
        "aa".into(),
        json!({"vout": [{
            "n": 0,
            "valueZat": 50_000_000,
            "scriptPubKey": {"addresses": ["tmABC"]}
        }]}),
    )]);
    assert!(!attach_transparent_prevouts(&mut tx, &prev));
    assert_eq!(tx["vin"][0]["valueZat"], 50_000_000);
    assert!(tx["vin"][0].get("prevout_unavailable").is_none());
    assert_eq!(tx["vin"][1]["prevout_unavailable"], true);
    assert!(tx["vin"][1].get("valueZat").is_none());
    assert!(tx["vin"][1].get("scriptPubKey").is_none());
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
async fn search_rejects_an_empty_query_before_calling_zakura() {
    let (node, calls) = node_that_records_calls().await;
    let (state, _dir) = state_at(node);
    for uri in [
        "/api/v1/search?q=",
        "/api/v1/search?q=%20",
        "/api/v1/search?q=%0A",
        "/api/v1/search?q=%20%09%0A",
    ] {
        assert_bad_request(state.clone(), uri, EMPTY_SEARCH).await;
        assert!(calls.lock().unwrap().is_empty(), "{uri}");
    }
}

#[tokio::test]
async fn explorer_rejects_untrimmed_addresses_before_calling_zakura() {
    let (node, calls) = node_that_records_calls().await;
    let (state, _dir) = state_at(node);
    let address = state.0.store.account(1).unwrap().transparent_address;
    for uri in [
        format!("/api/v1/addresses/{address}%0A"),
        format!("/api/v1/addresses/%20{address}"),
        format!("/api/v1/search?q={address}%0A"),
        format!("/api/v1/search?q=%20{address}"),
    ] {
        assert_bad_request(state.clone(), &uri, UNTRIMMED_ADDRESS).await;
        assert!(calls.lock().unwrap().is_empty(), "{uri}");
    }

    for uri in [
        format!("/api/v1/addresses/{address}"),
        format!("/api/v1/search?q={address}"),
    ] {
        calls.lock().unwrap().clear();
        let response = router(state.clone())
            .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["getaddressbalance"],
            "{uri}"
        );
    }

    calls.lock().unwrap().clear();
    let response = router(state)
        .oneshot(
            Request::get("/api/v1/search?q=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(calls.lock().unwrap().as_slice(), ["getblock"]);
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
async fn transaction_reports_incomplete_inputs_when_a_prevout_lookup_fails() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node = format!("http://{}", listener.local_addr().unwrap());
    let spending_txid = "b".repeat(64);
    let previous_txid = "a".repeat(64);
    let spending = spending_txid.clone();
    let previous = previous_txid.clone();
    let rpc = Router::new().route(
        "/",
        post(move |Json(req): Json<Value>| {
            let spending = spending.clone();
            let previous = previous.clone();
            async move {
                if req["method"] == "getrawtransaction" && req["params"][0] == spending {
                    Json(json!({
                        "result": {
                            "txid": spending,
                            "vin": [{"txid": previous, "vout": 0}],
                            "vout": []
                        },
                        "error": null,
                        "id": req["id"]
                    }))
                } else {
                    Json(json!({
                        "result": null,
                        "error": {"code": -5, "message": "hidden by test proxy"},
                        "id": req["id"]
                    }))
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, rpc).await });

    let (state, _dir) = state_with_local_wallet();
    let state = AppState::new(
        state.0.store.clone(),
        state.0.wallet.clone(),
        node,
        "test".into(),
    );
    let response = router(state)
        .oneshot(
            Request::get(format!("/api/v1/transactions/{spending_txid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let tx: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(tx["transparent_inputs_complete"], false);
    assert_eq!(tx["vin"][0]["prevout_unavailable"], true);
    assert_eq!(tx["vin"][0]["txid"], previous_txid);
    assert_eq!(tx["vin"][0]["vout"], 0);
    assert!(tx["vin"][0].get("valueZat").is_none());
    assert!(tx["vin"][0].get("scriptPubKey").is_none());
}
