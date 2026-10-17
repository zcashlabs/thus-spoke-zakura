use super::*;
use crate::db::{Store, TREASURY_ACCOUNT_ID, USER_ACCOUNT_COUNT};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn wallet_migration_installs_only_the_treasury_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let seed = SecretVec::new(hex::decode(store.seed().unwrap()).unwrap());
    let mut db = WalletDb::for_path(
        dir.path().join("wallet.db"),
        regtest_network(),
        SystemClock,
        UnwrapErr(SysRng),
    )
    .unwrap();
    zcash_client_sqlite::wallet::init::init_wallet_db(
        &mut db,
        Some(SecretVec::new(seed.expose_secret().clone())),
    )
    .unwrap();
    let birthday =
        AccountBirthday::from_parts(ChainState::empty(1.into(), BlockHash([0; 32])), None);
    for id in 1..=USER_ACCOUNT_COUNT {
        db.create_account(&format!("Account {id}"), &seed, &birthday, None)
            .unwrap();
    }
    drop(db);

    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    wallet
        .with_db(|db| {
            assert_eq!(db.get_account_ids()?.len(), 6);
            let (cursor, prepared): (u32, u32) =
                db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
                    Ok((
                        ext.query_row(
                            "SELECT COUNT(*) FROM sqlite_master WHERE name='ext_tsz_treasury_cursor'",
                            [],
                            |row| row.get(0),
                        )?,
                        ext.query_row(
                            "SELECT COUNT(*) FROM sqlite_master WHERE name='ext_tsz_prepared_transactions'",
                            [],
                            |row| row.get(0),
                        )?,
                    ))
                })?;
            assert_eq!((cursor, prepared), (1, 0));
            Ok(())
        })
        .await
        .unwrap();
    drop(wallet);
    let reopened = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    assert!(reopened.treasury_cursor().await.unwrap().is_none());
}

#[tokio::test]
async fn routine_transparent_queries_exclude_the_treasury() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();

    let queries = wallet.public_transparent_queries().await.unwrap();
    assert_eq!(queries.len(), usize::from(USER_ACCOUNT_COUNT));
    for id in 1..=USER_ACCOUNT_COUNT {
        assert!(queries.iter().any(|query| {
            query
                .addresses
                .contains(&store.account(id).unwrap().transparent_address)
        }));
    }
    assert!(!queries.iter().any(|query| {
        query.addresses.contains(
            &store
                .account(TREASURY_ACCOUNT_ID)
                .unwrap()
                .transparent_address,
        )
    }));
}

#[tokio::test]
async fn height_wait_deadline_includes_stalled_lightwalletd_calls() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let mut wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    wallet.lightwalletd = format!("http://{}", listener.local_addr().unwrap());

    let target = ChainCheckpoint {
        height: 1,
        hash: BlockHash([1; 32]).to_string(),
    };
    let result = tokio::time::timeout(
        Duration::from_millis(250),
        wallet.wait_for_block(&target, Duration::from_millis(50), |_| async {
            Ok(Some(BlockHash([1; 32]).to_string()))
        }),
    )
    .await;

    assert!(result.is_ok(), "block wait ignored its deadline");
    let error = result.unwrap().unwrap_err();
    assert!(
        error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
    );
}

/// A lightwalletd that answers `GetBlock` with the `n`th entry of `blocks` for its `n`th
/// request (repeating the last entry): `None` is the `OutOfRange` status lightwalletd returns
/// while Zakura lacks the height, `Some(hash)` a cached CompactBlock with that hash.
fn lightwalletd_blocks(
    listener: tokio::net::TcpListener,
    blocks: Vec<Option<[u8; 32]>>,
) -> (RealWallet, tempfile::TempDir, Arc<AtomicUsize>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let mut wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    wallet.lightwalletd = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let served = requests.clone();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut connection = h2::server::handshake(socket).await.unwrap();
        while let Some(request) = connection.accept().await {
            let (request, mut response) = request.unwrap();
            assert_eq!(
                request.uri().path(),
                "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlock"
            );
            let index = served.fetch_add(1, Ordering::SeqCst).min(blocks.len() - 1);
            let mut stream = response
                .send_response(
                    axum::http::Response::builder()
                        .status(200)
                        .header("content-type", "application/grpc")
                        .body(())
                        .unwrap(),
                    false,
                )
                .unwrap();
            let mut trailers = axum::http::HeaderMap::new();
            match blocks[index] {
                Some(hash) => {
                    // gRPC frame containing CompactBlock: height=1, hash.
                    let mut frame = vec![0, 0, 0, 0, 36, 0x10, 1, 0x1a, 32];
                    frame.extend_from_slice(&hash);
                    stream
                        .send_data(axum::body::Bytes::from(frame), false)
                        .unwrap();
                    trailers.insert("grpc-status", "0".parse().unwrap());
                }
                None => {
                    trailers.insert("grpc-status", "11".parse().unwrap());
                }
            }
            stream.send_trailers(trailers).unwrap();
        }
    });
    (wallet, dir, requests)
}

#[tokio::test]
async fn block_wait_outlasts_a_stale_lightwalletd_cache() {
    // After a rollback lightwalletd still serves the removed block at the mined height, and it
    // reports the height as indexed because `GetLatestBlock` answers from Zakura. The wait must
    // hold until the cache serves the block Zakura actually has.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (wallet, _dir, requests) =
        lightwalletd_blocks(listener, vec![None, Some([1; 32]), Some([2; 32])]);
    let target = ChainCheckpoint {
        height: 1,
        hash: BlockHash([2; 32]).to_string(),
    };

    wallet
        .wait_for_block(&target, Duration::from_secs(5), |height| async move {
            assert_eq!(height, 1);
            Ok(Some(BlockHash([2; 32]).to_string()))
        })
        .await
        .unwrap();

    assert_eq!(requests.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn block_wait_stops_when_zakura_replaces_the_block() {
    // The background sync may be waiting for a block that a reorg replaces meanwhile; waiting
    // for the full deadline would hold the wallet lock the reorg needs for its own sync.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (wallet, _dir, requests) = lightwalletd_blocks(listener, vec![Some([1; 32])]);
    let target = ChainCheckpoint {
        height: 1,
        hash: BlockHash([2; 32]).to_string(),
    };

    let error = tokio::time::timeout(
        Duration::from_millis(500),
        wallet.wait_for_block(&target, Duration::from_secs(5), |_| async {
            Ok(Some(BlockHash([1; 32]).to_string()))
        }),
    )
    .await
    .expect("block wait kept waiting for a replaced block")
    .unwrap_err();

    assert!(error.to_string().contains("replaced block"), "{error:#}");
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn block_wait_stops_when_zakura_rolls_back_below_the_height() {
    // A rollback that removes the target height leaves lightwalletd answering `OutOfRange` until
    // the chain grows back. The wait must end once Zakura no longer has the height instead of
    // holding the wallet lock the rollback's own sync needs.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (wallet, _dir, requests) = lightwalletd_blocks(listener, vec![None]);
    let target = ChainCheckpoint {
        height: 1,
        hash: BlockHash([1; 32]).to_string(),
    };

    let error = tokio::time::timeout(
        Duration::from_millis(500),
        wallet.wait_for_block(&target, Duration::from_secs(5), |_| async { Ok(None) }),
    )
    .await
    .expect("block wait kept waiting for a rolled back height")
    .unwrap_err();

    assert!(error.to_string().contains("rolled back below"), "{error:#}");
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn database_work_does_not_block_the_async_executor() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let started = std::time::Instant::now();
    let work = tokio::spawn(async move {
        wallet
            .with_db(move |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(())
            })
            .await
    });
    entered_rx.await.unwrap();
    let elapsed = started.elapsed();
    release_tx.send(()).unwrap();
    work.await.unwrap().unwrap();
    assert!(
        elapsed < Duration::from_millis(750),
        "database work blocked the async executor for {elapsed:?}"
    );
}
