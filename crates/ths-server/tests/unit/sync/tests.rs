use super::*;
use crate::state::tests::{counting_rpc, state_with_local_wallet};
use crate::{
    db::Store,
    rpc::{ChainCheckpoint, NodeRpc},
    wallet::{RealWallet, WALLET_BIRTHDAY_HEIGHT},
};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[tokio::test]
async fn a_scan_behind_the_tip_publishes_only_while_it_is_canonical() {
    let checkpoint = |height, hash: &str| ChainCheckpoint {
        height,
        hash: hash.into(),
    };
    let (target, tip) = (checkpoint(10, "a"), checkpoint(12, "c"));
    let publishable = |scanned: Option<ChainCheckpoint>, canonical: &'static str| {
        let (target, tip) = (target.clone(), tip.clone());
        async move {
            scan_is_publishable(scanned.as_ref(), &target, &tip, |_| async move {
                Ok(canonical.to_owned())
            })
            .await
            .unwrap()
        }
    };
    assert!(publishable(Some(checkpoint(12, "c")), "c").await);
    // mined past after reaching the target.
    assert!(publishable(Some(checkpoint(10, "a")), "a").await);
    assert!(publishable(Some(checkpoint(11, "b")), "b").await);
    // reorganized.
    assert!(!publishable(Some(checkpoint(10, "a")), "x").await);
    // short of the target.
    assert!(!publishable(Some(checkpoint(9, "z")), "z").await);
    assert!(!publishable(Some(checkpoint(12, "z")), "c").await);
    assert!(!publishable(Some(checkpoint(13, "d")), "c").await);
    assert!(!publishable(None, "c").await);
    // the tip dropped below the target.
    assert!(
        !scan_is_publishable(
            Some(&checkpoint(10, "a")),
            &checkpoint(12, "c"),
            &checkpoint(10, "a"),
            |_| async { unreachable!() },
        )
        .await
        .unwrap()
    );
    let below_birthday = checkpoint(u64::from(WALLET_BIRTHDAY_HEIGHT) - 1, "a");
    assert!(
        scan_is_publishable(None, &below_birthday, &below_birthday, |_| async {
            unreachable!()
        })
        .await
        .unwrap()
    );
}

#[tokio::test]
async fn publication_keeps_the_last_snapshot_unless_the_scan_reached_its_target() {
    use zcash_client_backend::{
        data_api::{
            chain::ChainState,
            scanning::{ScanPriority, ScanRange},
        },
        proto::compact_formats::CompactBlock,
    };
    use zcash_primitives::block::BlockHash;

    let hash = |height: u64| BlockHash([height as u8; 32]).to_string();
    // the node's tip height and its block hash at each height.
    let node = Arc::new(Mutex::new((4_u64, (0..=6).map(hash).collect::<Vec<_>>())));
    let rpc_node = node.clone();
    let rpc = Router::new().route(
        "/",
        post(move |Json(request): Json<Value>| {
            let node = rpc_node.clone();
            async move {
                let (tip, hashes) = node.lock().unwrap().clone();
                let result = match request["method"].as_str().unwrap() {
                    "getblockchaininfo" => {
                        json!({"chain":"regtest","blocks":tip,"bestblockhash":hashes[tip as usize]})
                    }
                    "getblockhash" => {
                        json!(hashes[request["params"][0].as_u64().unwrap() as usize])
                    }
                    method => panic!("unexpected rpc {method}"),
                };
                Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result,"error":null}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, rpc).await.unwrap() });
    let (mut state, _dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(endpoint);

    let wallet = &state.0.wallet;
    wallet.update_chain_tip(4).await.unwrap();
    let blocks = (2..=4)
        .map(|height| CompactBlock {
            height,
            hash: vec![height as u8; 32],
            prev_hash: vec![(height - 1) as u8; 32],
            chain_metadata: Some(Default::default()),
            ..Default::default()
        })
        .collect();
    wallet
        .scan_batch(
            ScanRange::from_parts(2.into()..5.into(), ScanPriority::Historic),
            blocks,
            ChainState::empty(1.into(), BlockHash([1; 32])),
        )
        .await
        .unwrap();
    let target = |height| ChainCheckpoint {
        height,
        hash: hash(height),
    };

    // the scan reached its target and the tip moved on.
    node.lock().unwrap().0 = 6;
    state.refresh_wallet_snapshot(&target(4)).await.unwrap();
    let published = state.wallet_sync_status().await;
    assert_eq!(
        (published.fully_scanned_height, published.observed_height),
        (Some(4), Some(6))
    );
    state.0.wallet_snapshot.write().await.accounts[0].ironwood_zatoshi = 400_000_000;

    // short of the target, orphaned, and a tip that dropped below the target.
    for (target_height, tip, scanned_hash) in
        [(5, 6, hash(4)), (4, 6, "ee".repeat(32)), (6, 4, hash(4))]
    {
        {
            let mut node = node.lock().unwrap();
            node.0 = tip;
            node.1[4] = scanned_hash;
        }
        assert!(
            state
                .refresh_wallet_snapshot(&target(target_height))
                .await
                .is_err()
        );
        let snapshot = state.0.wallet_snapshot.read().await;
        assert_eq!(snapshot.accounts[0].ironwood_zatoshi, 400_000_000);
        assert_eq!(
            (
                snapshot.status.fully_scanned_height,
                snapshot.status.observed_height,
                snapshot.status.last_success_at,
            ),
            (
                published.fully_scanned_height,
                published.observed_height,
                published.last_success_at,
            )
        );
    }
}

#[tokio::test]
async fn synchronization_failure_preserves_the_last_good_snapshot() {
    let (state, _dir) = state_with_local_wallet();
    {
        let mut snapshot = state.0.wallet_snapshot.write().await;
        snapshot.accounts[0].ironwood_zatoshi = 400_000_000;
        snapshot.status.last_success_at = Some(123);
    }

    assert!(state.synchronize_wallet(Some(1)).await.is_err());

    let snapshot = state.0.wallet_snapshot.read().await;
    assert_eq!(snapshot.accounts[0].ironwood_zatoshi, 400_000_000);
    assert_eq!(snapshot.status.last_success_at, Some(123));
    assert_eq!(snapshot.status.state, "error");
    assert!(snapshot.status.error.is_some());
}

/// A lightwalletd that answers `GetLatestBlock` with a tip at height 101 while `up` is set and
/// with `UNAVAILABLE` otherwise, on every connection the server opens.
fn lightwalletd_tip(listener: tokio::net::TcpListener, up: Arc<AtomicBool>) {
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let up = up.clone();
            tokio::spawn(async move {
                let mut connection = h2::server::handshake(socket).await.unwrap();
                while let Some(request) = connection.accept().await {
                    let (request, mut response) = request.unwrap();
                    assert_eq!(
                        request.uri().path(),
                        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLatestBlock"
                    );
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
                    if up.load(Ordering::SeqCst) {
                        // gRPC frame containing BlockID: height=101, hash.
                        let mut frame = vec![0, 0, 0, 0, 36, 0x08, 101, 0x12, 32];
                        frame.extend_from_slice(&[0xaa; 32]);
                        stream
                            .send_data(axum::body::Bytes::from(frame), false)
                            .unwrap();
                        trailers.insert("grpc-status", "0".parse().unwrap());
                    } else {
                        trailers.insert("grpc-status", "14".parse().unwrap());
                    }
                    stream.send_trailers(trailers).unwrap();
                }
            });
        }
    });
}

#[tokio::test]
async fn lightwalletd_reachability_changes_emit_a_sync_event() {
    // The status handler probes lightwalletd on each request, but with the chain idle nothing
    // told an open dashboard to ask again, so its health label kept the last answer.
    let (rpc, _calls) = counting_rpc(true).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let lightwalletd = format!("http://{}", listener.local_addr().unwrap());
    let up = Arc::new(AtomicBool::new(true));
    lightwalletd_tip(listener, up.clone());
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap())
        .unwrap()
        .with_lightwalletd(lightwalletd);
    let state = AppState::new(store, wallet, rpc, "test".into());
    {
        // the wallet already matches the node's tip, so a tick has nothing to scan
        let mut snapshot = state.0.wallet_snapshot.write().await;
        snapshot.status.state = "ready";
        snapshot.status.fully_scanned_height = Some(101);
        snapshot.status.fully_scanned_hash = Some("a".repeat(64));
    }
    let mut events = state.0.events.subscribe();
    let mut topics = || std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();

    state.sync_if_chain_advanced().await.unwrap();
    assert!(
        topics().is_empty(),
        "a tick with nothing new emitted an event"
    );

    up.store(false, Ordering::SeqCst);
    state.sync_if_chain_advanced().await.unwrap();
    assert_eq!(topics(), ["sync"], "lightwalletd loss went unannounced");
    state.sync_if_chain_advanced().await.unwrap();
    assert!(
        topics().is_empty(),
        "an unchanged outage was announced again"
    );

    up.store(true, Ordering::SeqCst);
    state.sync_if_chain_advanced().await.unwrap();
    assert_eq!(topics(), ["sync"], "lightwalletd return went unannounced");
}
