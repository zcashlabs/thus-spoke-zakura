use super::*;
use futures_util::stream;
use transparent::bundle::OutPoint;
use zcash_primitives::block::BlockHash;

use crate::wallet::transparent_history_test_support as support;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}
fn checkpoint() -> ChainCheckpoint {
    ChainCheckpoint {
        height: 5,
        hash: BlockHash([5; 32]).to_string(),
    }
}

#[tokio::test]
async fn completed_history_imports_external_transparent_only_spends_at_both_range_boundaries() {
    let (_dir, store, wallet) = support::setup(5).await;
    let receiver = store.account(2).unwrap().transparent_address;
    let query = wallet
        .transparent_history_queries()
        .await
        .unwrap()
        .into_iter()
        .find(|q| q.address == receiver)
        .unwrap();
    let funding = support::transaction(&receiver, OutPoint::new([9; 32], 0), 1, 2);
    let tx = HistoryTransaction::decode(funding.clone(), 2, 5)
        .await
        .unwrap();
    let spend = support::transaction(
        &store.account(3).unwrap().transparent_address,
        OutPoint::new(*tx.transaction.txid().as_ref(), 0),
        2,
        5,
    );
    ingest_range(
        &wallet,
        query,
        checkpoint(),
        stream::iter([Ok(funding), Ok(spend)]),
        |_| async { Ok(checkpoint().hash) },
        |_, _| async { Ok(()) },
        deadline(),
    )
    .await
    .unwrap();
    let mut accounts = store.accounts().unwrap();
    wallet.apply_balances(&mut accounts).await.unwrap();
    assert_eq!(accounts[1].transparent_zatoshi, 0);
    assert_eq!(accounts[2].transparent_zatoshi, 10_000_000);
    assert_eq!(
        wallet.transparent_history_checkpoints().await.unwrap(),
        vec![checkpoint()]
    );
}

#[tokio::test]
async fn interrupted_stream_keeps_coverage_behind_committed_chunks_and_retries_idempotently() {
    let (dir, store, wallet) = support::setup(5).await;
    let receiver = store.account(2).unwrap().transparent_address;
    let query = wallet
        .transparent_history_queries()
        .await
        .unwrap()
        .into_iter()
        .find(|q| q.address == receiver)
        .unwrap();
    let funding = support::transaction(&receiver, OutPoint::new([9; 32], 0), 1, 2);
    let replies = (0..SCAN_BATCH_SIZE)
        .map(|_| Ok(funding.clone()))
        .chain([Err(tonic::Status::unavailable(
            "injected stream interruption",
        ))])
        .collect::<Vec<_>>();
    assert!(
        ingest_range(
            &wallet,
            query.clone(),
            checkpoint(),
            stream::iter(replies),
            |_| async { Ok(checkpoint().hash) },
            |_, _| async { Ok(()) },
            deadline()
        )
        .await
        .is_err()
    );
    assert!(
        wallet
            .transparent_history_checkpoints()
            .await
            .unwrap()
            .is_empty()
    );
    let mut accounts = store.accounts().unwrap();
    wallet.apply_balances(&mut accounts).await.unwrap();
    assert_eq!(accounts[1].transparent_zatoshi, 10_000_000);
    ingest_range(
        &wallet,
        query,
        checkpoint(),
        stream::iter([Ok(funding)]),
        |_| async { Ok(checkpoint().hash) },
        |_, _| async { Ok(()) },
        deadline(),
    )
    .await
    .unwrap();
    let count: u32 = rusqlite::Connection::open(dir.path().join("wallet.db"))
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM transparent_received_outputs",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        wallet.transparent_history_checkpoints().await.unwrap(),
        vec![checkpoint()]
    );
}

#[tokio::test]
async fn empty_successful_history_keeps_unspent_outputs_and_advances_coverage() {
    let (_dir, store, wallet) = support::setup(5).await;
    let receiver = store.account(2).unwrap().transparent_address;
    let raw = support::transaction(&receiver, OutPoint::new([9; 32], 0), 1, 2);
    wallet
        .import_transparent_history(vec![HistoryTransaction::decode(raw, 2, 5).await.unwrap()])
        .await
        .unwrap();
    let query = wallet
        .transparent_history_queries()
        .await
        .unwrap()
        .into_iter()
        .find(|q| q.address == receiver)
        .unwrap();
    ingest_range(
        &wallet,
        query,
        checkpoint(),
        stream::empty(),
        |_| async { Ok(checkpoint().hash) },
        |_, _| async { Ok(()) },
        deadline(),
    )
    .await
    .unwrap();
    let mut accounts = store.accounts().unwrap();
    wallet.apply_balances(&mut accounts).await.unwrap();
    assert_eq!(accounts[1].transparent_zatoshi, 10_000_000);
    assert_eq!(
        wallet.transparent_history_checkpoints().await.unwrap(),
        vec![checkpoint()]
    );
}

#[tokio::test]
async fn fork_bad_transaction_or_deadline_never_advances_history_coverage() {
    let (_dir, store, wallet) = support::setup(5).await;
    let receiver = store.account(2).unwrap().transparent_address;
    let query = wallet
        .transparent_history_queries()
        .await
        .unwrap()
        .into_iter()
        .find(|q| q.address == receiver)
        .unwrap();
    assert!(
        ingest_range(
            &wallet,
            query.clone(),
            checkpoint(),
            stream::empty(),
            |_| async { Ok("ff".repeat(32)) },
            |_, _| async { Ok(()) },
            deadline()
        )
        .await
        .is_err()
    );
    let raw = support::transaction(&receiver, OutPoint::new([9; 32], 0), 1, 2);
    assert!(
        ingest_range(
            &wallet,
            query.clone(),
            checkpoint(),
            stream::iter([Ok(raw)]),
            |_| async { Ok(checkpoint().hash) },
            |_, _| async { anyhow::bail!("not canonical") },
            deadline()
        )
        .await
        .is_err()
    );
    assert!(
        ingest_range(
            &wallet,
            query,
            checkpoint(),
            stream::pending(),
            |_| async { Ok(checkpoint().hash) },
            |_, _| async { Ok(()) },
            Instant::now() + Duration::from_millis(30)
        )
        .await
        .is_err()
    );
    assert!(
        wallet
            .transparent_history_checkpoints()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn canonical_history_validation_retries_missing_unmined_or_reorged_transactions() {
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    let txid = "11".repeat(32);
    let canonical = "03".repeat(32);
    for (height, confirmations, blockhash, missing, valid) in [
        (3, 1, canonical.clone(), false, true),
        (3, 0, canonical.clone(), false, false),
        (4, 1, canonical.clone(), false, false),
        (3, 1, "ff".repeat(32), false, false),
        (3, 1, canonical.clone(), true, false),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rpc = NodeRpc::new(format!("http://{}", listener.local_addr().unwrap()));
        let id = txid.clone();
        let hash = canonical.clone();
        let app=Router::new().route("/",post(move |Json(request):Json<Value>| {
            let id=id.clone();let hash=hash.clone();let blockhash=blockhash.clone();
            async move {
                if request["method"]=="getblockhash" {
                    Json(json!({"result":hash,"error":null}))
                } else if missing {
                    Json(json!({"result":null,"error":{"code":-5,"message":"not found"}}))
                } else {
                    Json(json!({"result":{"txid":id,"height":height,"confirmations":confirmations,"blockhash":blockhash},"error":null}))
                }
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let result = verify_transaction(&rpc, txid.clone(), 3).await;
        if valid {
            result.unwrap();
        } else {
            let error = result.unwrap_err();
            assert!(
                error
                    .downcast_ref::<crate::bootstrap::ReadinessError>()
                    .is_some()
            );
            assert!(crate::bootstrap::startup_retryable(&error));
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn stale_compact_checkpoint_retries_before_requesting_or_importing_history() {
    let (_dir, _store, wallet) = support::setup(5).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc = NodeRpc::new(format!("http://{}", listener.local_addr().unwrap()));
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(|| async {
            axum::Json(serde_json::json!({"result":"ff".repeat(32),"error":null}))
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // This endpoint cannot serve history. A stale scan must return before any
    // lightwalletd connection or mutation, instead of failing later at commit.
    let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
    let mut client = CompactTxStreamerClient::new(channel);
    let error = synchronize(&wallet, &rpc, &mut client, deadline())
        .await
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<crate::bootstrap::ReadinessError>()
            .is_some()
    );
    assert!(crate::bootstrap::startup_retryable(&error));
    assert!(
        wallet
            .transparent_history_checkpoints()
            .await
            .unwrap()
            .is_empty()
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}
