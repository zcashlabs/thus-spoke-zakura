use super::*;
use crate::db::Store;
use crate::payments::progression::payment_confirmation_loop;
use crate::payments::submission::submit_prepared;
use crate::rpc::NodeRpc;
use serde_json::json;
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
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
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

use crate::state::tests::state_with_local_wallet;
use crate::wallet::RealWallet;
use axum::{Json, Router, routing::post};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::broadcast;

pub(crate) fn lifecycle_claim(store: &Store, key: &str) -> Activity {
    store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            key,
            None,
        )
        .unwrap()
}

fn confirmation_claim(store: &Store, key: &str) -> Activity {
    let claim = lifecycle_claim(store, key);
    let prepared = store
        .record_prepared(&claim.id, key, b"signed", 140)
        .unwrap();
    store.mark_broadcast(&prepared.id, key).unwrap()
}

#[tokio::test]
async fn payment_confirmation_loop_recovers_reopened_records_without_wallet_sync() {
    let (state, dir) = state_with_local_wallet();
    confirmation_claim(&state.0.store, "reopened-txid");
    drop(state);
    let calls = Arc::new(AtomicUsize::new(0));
    let rpc_calls = calls.clone();
    let rpc = Router::new().route("/", post(move |Json(request): Json<Value>| {
            let calls = rpc_calls.clone();
            async move {
                let response = match request["method"].as_str().unwrap() {
                    "getrawtransaction" => {
                        assert_eq!(request["params"][0], "reopened-txid");
                        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                            json!({"result": null, "error": {"code": -5, "message": "not found"}, "id": request["id"]})
                        } else {
                            json!({"result": {"txid": "reopened-txid", "confirmations": 1, "blockhash": "inclusion-block"}, "error": null, "id": request["id"]})
                        }
                    }
                    "getblockchaininfo" => json!({"result": {"chain": "regtest", "blocks": 100, "bestblockhash": "tip-block"}, "error": null, "id": request["id"]}),
                    "getblock" => {
                        assert_eq!(request["params"][0], "inclusion-block");
                        json!({"result": {"height": 100}, "error": null, "id": request["id"]})
                    }
                    "getblockhash" => {
                        assert_eq!(request["params"][0], 100);
                        json!({"result": "inclusion-block", "error": null, "id": request["id"]})
                    }
                    method => panic!("unexpected recovery RPC {method}"),
                };
                Json(response)
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, rpc).await.unwrap() });
    let store = Store::open(dir.path().join("server.db")).unwrap();
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    let state = AppState::new(store, wallet, rpc_url, "test".into());
    state.mark_started();
    let mut updates = state.0.events.subscribe();
    // The production loop must progress even while synchronization owns
    // its guard, and must not require a new block or a payment POST.
    let _sync = state.0.wallet_sync.lock().await;
    let worker = tokio::spawn(payment_confirmation_loop(state.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        assert_eq!(updates.recv().await.unwrap(), "wallet");
    })
    .await
    .unwrap();
    let confirmed = state
        .0
        .store
        .activity_for_key("reopened-txid")
        .unwrap()
        .unwrap();
    assert_eq!(confirmed.status, "confirmed");
    assert_eq!(confirmed.txid, "reopened-txid");
    assert_eq!(confirmed.block_hash.as_deref(), Some("inclusion-block"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn payment_confirmation_loop_survives_request_confirmation_cancellation() {
    let (state, dir) = state_with_local_wallet();
    let claim = confirmation_claim(&state.0.store, "cancelled-request-txid");
    let store = state.0.store.clone();
    drop(state);
    let (entered, lookup_started) = tokio::sync::oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered)));
    let (release_lookup, released) = tokio::sync::oneshot::channel();
    let released = Arc::new(Mutex::new(Some(released)));
    let calls = Arc::new(AtomicUsize::new(0));
    let rpc_calls = calls.clone();
    let rpc = Router::new().route("/", post(move |Json(request): Json<Value>| {
            let entered = entered.clone();
            let released = released.clone();
            let calls = rpc_calls.clone();
            async move {
                match request["method"].as_str().unwrap() {
                    "getrawtransaction" => {}
                    "getblock" => {
                        assert_eq!(request["params"][0], "recovered-block");
                        return Json(json!({"result": {"height": 100}, "error": null, "id": request["id"]}));
                    }
                    "getblockhash" => {
                        assert_eq!(request["params"][0], 100);
                        return Json(json!({"result": "recovered-block", "error": null, "id": request["id"]}));
                    }
                    method => panic!("unexpected recovery RPC {method}"),
                }
                assert_eq!(request["params"][0], "cancelled-request-txid");
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    let released = released.lock().unwrap().take().unwrap();
                    entered.lock().unwrap().take().unwrap().send(()).unwrap();
                    // Cancellation happens after the request-side lookup starts,
                    // before it can confirm, auto-mine, or notify subscribers.
                    tokio::time::timeout(Duration::from_secs(10), released)
                        .await
                        .unwrap()
                        .unwrap();
                }
                Json(json!({
                    "result": {"txid": "cancelled-request-txid", "confirmations": 1, "blockhash": "recovered-block"},
                    "error": null,
                    "id": request["id"]
                }))
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, rpc).await.unwrap() });
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    let state = AppState::new(store, wallet, rpc_url, "test".into());
    state.mark_started();
    let mut updates = state.0.events.subscribe();
    let request_state = state.clone();
    let request_claim = claim.clone();
    let request =
        tokio::spawn(async move { confirm_after_mining(&request_state, request_claim).await });
    let lookup_started = tokio::time::timeout(Duration::from_secs(5), lookup_started).await;
    request.abort();
    let request_result = request.await;
    let lookup_released = release_lookup.send(());
    let pending = state.0.store.activity_for_key("cancelled-request-txid");
    let signed = state.0.store.prepared_transaction(&claim.id);
    let no_request_update = updates.try_recv();
    // Request cancellation must not cancel server-owned work. Holding the
    // sync guard also proves recovery does not depend on wallet scanning.
    let _sync = state.0.wallet_sync.lock().await;
    let worker = tokio::spawn(payment_confirmation_loop(state.clone()));
    let update = tokio::time::timeout(Duration::from_secs(5), updates.recv()).await;
    worker.abort();
    let worker_result = worker.await;
    server.abort();
    let server_result = server.await;

    lookup_started.unwrap().unwrap();
    lookup_released.unwrap();
    assert!(request_result.unwrap_err().is_cancelled());
    let pending = pending.unwrap().unwrap();
    assert_eq!(pending.id, claim.id);
    assert_eq!(pending.txid, claim.txid);
    assert_eq!(pending.status, "broadcast");
    assert!(pending.block_hash.is_none());
    assert_eq!(signed.unwrap().raw_transaction, b"signed");
    assert!(matches!(
        no_request_update,
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(update.unwrap().unwrap(), "wallet");
    let confirmed = state
        .0
        .store
        .activity_for_key("cancelled-request-txid")
        .unwrap()
        .unwrap();
    assert_eq!(confirmed.id, claim.id);
    assert_eq!(confirmed.txid, claim.txid);
    assert_eq!(confirmed.status, "confirmed");
    assert_eq!(confirmed.block_hash.as_deref(), Some("recovered-block"));
    assert_eq!(
        state
            .0
            .store
            .prepared_transaction(&claim.id)
            .unwrap()
            .raw_transaction,
        b"signed"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(worker_result.unwrap_err().is_cancelled());
    assert!(server_result.unwrap_err().is_cancelled());
}

use crate::rpc::RpcError;
use crate::state::tests::api_request;
use axum::http::StatusCode;
#[tokio::test]
async fn startup_confirmation_preserves_fatal_mining_errors_and_normal_post_pending_behavior() {
    for mode in ["warmup", "internal", "malformed-block", "store"] {
        let (local, _dir) = state_with_local_wallet();
        let app = Router::new().route("/", post(move |Json(request): Json<Value>| async move {
                let result = match request["method"].as_str().unwrap() {
                    "getrawtransaction" if mode == "store" => json!({"txid":"original-startup-txid", "confirmations":1, "blockhash":"confirmed-block"}),
                    "getrawtransaction" => json!({"txid":"original-startup-txid", "confirmations":0}),
                    "generate" if mode == "warmup" => return Json(json!({"error":{"code":-28,"message":"warming up"},"result":null})),
                    "generate" if mode == "internal" => return Json(json!({"error":{"code":-32603,"message":"internal failure"},"result":null})),
                    "generate" => json!(["mined-block"]),
                    "getblock" if mode == "store" => json!({"height":100}),
                    "getblockhash" if mode == "store" => json!("confirmed-block"),
                    "getblock" => json!({}),
                    method => panic!("unexpected startup method {method}"),
                };
                Json(json!({"result":result,"error":null}))
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = AppState::new(
            local.0.store.clone(),
            local.0.wallet.clone(),
            format!("http://{}", listener.local_addr().unwrap()),
            "test".into(),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let pending = state
            .0
            .store
            .claim_faucet(1, "ironwood", 500_000_000, "startup-account-1-ironwood-v1")
            .unwrap();
        let pending = state
            .0
            .store
            .record_prepared(
                &pending.id,
                "original-startup-txid",
                b"original signed bytes",
                140,
            )
            .unwrap();
        let pending = state
            .0
            .store
            .mark_broadcast(&pending.id, &pending.txid)
            .unwrap();
        if mode == "store" {
            rusqlite::Connection::open(_dir.path().join("server.db"))
                .unwrap()
                .execute("DROP TABLE activity", [])
                .unwrap();
        }
        let error = confirm_after_mining(&state, pending.clone())
            .await
            .unwrap_err();
        assert_eq!(
            crate::bootstrap::startup_retryable(&error),
            mode == "warmup",
            "{mode}: {error:#}"
        );
        if mode == "warmup" || mode == "internal" {
            assert_eq!(
                error.downcast_ref::<RpcError>().unwrap().code,
                if mode == "warmup" { -28 } else { -32603 }
            );
        }
        if mode == "store" {
            assert!(error.downcast_ref::<rusqlite::Error>().is_some());
        } else {
            let saved = state.0.store.prepared_transaction(&pending.id).unwrap();
            assert_eq!(saved.raw_transaction, b"original signed bytes");
            state.mark_started();
            let (status, response) = api_request(&state, "POST", "/api/v1/faucet", json!({"account_id":1,"pool":"ironwood","amount_zatoshi":500_000_000,"idempotency_key":"startup-account-1-ironwood-v1"})).await;
            assert_eq!(status, StatusCode::ACCEPTED);
            assert_eq!(response["status"], "broadcast");
            assert_eq!(response["txid"], "original-startup-txid");
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn canonical_payment_reads_share_the_remaining_attempt_deadline() {
    for stalled_method in ["getblock", "getblockhash"] {
        let (mut state, _dir) = state_with_local_wallet();
        let claim = lifecycle_claim(&state.0.store, stalled_method);
        let prepared = state
            .0
            .store
            .record_prepared(&claim.id, "original-txid", b"signed original", 140)
            .unwrap();
        let app = Router::new().route("/", post(move |Json(request): Json<Value>| async move {
                let method = request["method"].as_str().unwrap();
                if method == stalled_method { tokio::time::sleep(Duration::from_secs(5)).await; }
                let result = match method {
                    "getrawtransaction" => json!({"txid":"original-txid","confirmations":1,"blockhash":"original-block"}),
                    "getblock" => json!({"height":140}),
                    "getblockhash" => json!("original-block"),
                    _ => panic!("unexpected RPC"),
                };
                Json(json!({"result":result,"error":null}))
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        Arc::get_mut(&mut state.0).unwrap().rpc =
            NodeRpc::new(format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let attempt = PaymentAttempt {
            state: &state,
            dispatch_deadline: Some(Instant::now() + Duration::from_millis(100)),
        };
        let started = Instant::now();
        assert!(
            submit_prepared(&state.0.store, &attempt, &prepared)
                .await
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        let saved = state
            .0
            .store
            .activity_for_key(stalled_method)
            .unwrap()
            .unwrap();
        assert_eq!(saved.status, "prepared");
        assert_eq!(saved.txid, "original-txid");
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
