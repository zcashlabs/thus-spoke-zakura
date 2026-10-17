use super::*;
use crate::payments::confirmation::tests::lifecycle_claim;
use crate::payments::submission::PaymentSubmitter;
use crate::payments::treasury::FaucetRuntime;
use crate::rpc::NodeRpc;
use crate::state::tests::state_with_local_wallet;
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn automatic_mining_waits_for_chain_operation_within_the_dispatch_budget() {
    let (mut state, _dir) = state_with_local_wallet();
    let (url, calls) = crate::state::tests::counting_rpc(true).await;
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(url);
    let _chain = state.0.mining.chain_operations.write().await;
    let attempt = PaymentAttempt {
        state: &state,
        dispatch_deadline: Some(Instant::now() + Duration::from_millis(50)),
    };
    assert!(attempt.mine_and_sync(1).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn expired_attempt_observes_preparation_and_keeps_guards_until_recording_finishes() {
    let (state, dir) = state_with_local_wallet();
    let claim = lifecycle_claim(&state.0.store, "deadline-worker");
    rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap().execute(
            "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES(?1,'worker-txid',x'010203',140)", [&claim.id],
        ).unwrap();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let deadline = Instant::now() + Duration::from_millis(100);
    let work_state = state.clone();
    let id = claim.id.clone();
    let work = tokio::spawn(async move {
        let attempt = PaymentAttempt {
            state: &work_state,
            dispatch_deadline: Some(deadline),
        };
        let _payments = attempt.lock(&work_state.0.payments).await?;
        let _replenishment = attempt.lock(&work_state.0.treasury_replenishment).await?;
        let store = work_state.0.store.clone();
        let recorded_id = id.clone();
        let prepared = FaucetRuntime::prepare_payment(
            &attempt,
            Some(&id),
            None,
            "invalid seed",
            "invalid address",
            0,
            move |prepared| {
                entered.send(()).unwrap();
                released.recv().unwrap();
                store.record_prepared(
                    &recorded_id,
                    &prepared.txid,
                    &prepared.raw_transaction,
                    prepared.expiry_height,
                )?;
                Ok(prepared)
            },
        )
        .await?;
        PaymentSubmitter::broadcast(&attempt, &prepared.raw_transaction).await
    });
    ready.await.unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(!work.is_finished());
    assert!(state.0.payments.try_lock().is_err());
    assert!(state.0.treasury_replenishment.try_lock().is_err());
    release.send(()).unwrap();
    let error = work.await.unwrap().unwrap_err();
    assert!(
        error
            .downcast_ref::<crate::reconcile::ReconciliationDeadline>()
            .is_some()
    );
    let saved = state
        .0
        .store
        .activity_for_key("deadline-worker")
        .unwrap()
        .unwrap();
    assert_eq!(saved.status, "prepared");
    assert_eq!(saved.txid, "worker-txid");
    assert_eq!(
        state
            .0
            .store
            .prepared_transaction(&claim.id)
            .unwrap()
            .raw_transaction,
        [1, 2, 3]
    );
    assert!(state.0.payments.try_lock().is_ok());
    assert!(state.0.treasury_replenishment.try_lock().is_ok());
}

#[tokio::test]
async fn attempt_bounds_treasury_and_wallet_sync_serialization_waits() {
    let (state, _dir) = state_with_local_wallet();
    let _treasury = state.0.treasury_replenishment.lock().await;
    let _sync = state.0.wallet_sync.lock().await;
    let attempt = PaymentAttempt {
        state: &state,
        dispatch_deadline: Some(Instant::now() + Duration::from_millis(100)),
    };
    assert!(attempt.lock(&state.0.treasury_replenishment).await.is_err());
    assert!(attempt.synchronize_wallet(None).await.is_err());
    assert!(state.0.treasury_replenishment.try_lock().is_err());
    assert!(state.0.wallet_sync.try_lock().is_err());
}

#[tokio::test]
async fn startup_confirmation_keeps_its_stage_limit_while_dispatch_uses_remaining_time() {
    let (mut state, _dir) = state_with_local_wallet();
    let app = Router::new().route(
        "/",
        post(|| async {
            tokio::time::sleep(Duration::from_millis(2200)).await;
            Json(json!({"result":{"txid":"confirmation-txid","confirmations":0},"error":null}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    Arc::get_mut(&mut state.0).unwrap().rpc =
        NodeRpc::new(format!("http://{}", listener.local_addr().unwrap()));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let startup = PaymentAttempt {
        state: &state,
        dispatch_deadline: None,
    };
    let periodic = PaymentAttempt {
        state: &state,
        dispatch_deadline: Some(Instant::now() + Duration::from_millis(100)),
    };
    let (startup_result, periodic_result) = tokio::join!(
        startup.lookup_transaction("confirmation-txid"),
        periodic.lookup_transaction("confirmation-txid")
    );
    assert!(periodic_result.is_err());
    assert!(
        startup_result.unwrap().is_some(),
        "startup confirmation inherited the periodic two-second limit"
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn automatic_mining_stages_share_the_remaining_attempt_budget() {
    for stalled_stage in ["generate", "getblock"] {
        let (mut state, _dir) = state_with_local_wallet();
        let generated = Arc::new(AtomicUsize::new(0));
        let block_reads = Arc::new(AtomicUsize::new(0));
        let generated_rpc = generated.clone();
        let blocks_rpc = block_reads.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<Value>| {
                let generated = generated_rpc.clone();
                let blocks = blocks_rpc.clone();
                async move {
                    let method = request["method"].as_str().unwrap();
                    match method {
                        "generate" => {
                            generated.fetch_add(1, Ordering::SeqCst);
                        }
                        "getblock" => {
                            blocks.fetch_add(1, Ordering::SeqCst);
                        }
                        method => panic!("unexpected stage {method}"),
                    }
                    tokio::time::sleep(if method == stalled_stage {
                        Duration::from_secs(5)
                    } else {
                        Duration::from_millis(50)
                    })
                    .await;
                    Json(json!({"result":["mined-block"],"error":null}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        Arc::get_mut(&mut state.0).unwrap().rpc =
            NodeRpc::new(format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let attempt = PaymentAttempt {
            state: &state,
            dispatch_deadline: Some(Instant::now() + Duration::from_millis(150)),
        };
        let result = tokio::time::timeout(Duration::from_millis(500), attempt.mine_and_sync(1))
            .await
            .unwrap();
        assert!(result.is_err());
        assert_eq!(generated.load(Ordering::SeqCst), 1);
        assert_eq!(
            block_reads.load(Ordering::SeqCst),
            usize::from(stalled_stage == "getblock")
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
