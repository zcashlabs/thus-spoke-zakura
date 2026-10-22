use super::*;
use crate::api::router;
use crate::mining::MiningMode;
use crate::network::regtest_network;
use crate::state::tests::{state_with_local_wallet, state_with_mode};
use crate::wallet::PreparedPayment;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use transparent::{
    address::Script,
    bundle::{Authorized as TransparentAuthorized, Bundle, OutPoint, TxIn, TxOut},
};
use zcash_keys::address::Address;
use zcash_primitives::transaction::{Authorized, Transaction, TransactionData, TxVersion};
use zcash_protocol::{
    consensus::{BlockHeight, BranchId},
    value::Zatoshis,
};
use zcash_script::script;
use zcash_script::script::Evaluable;

fn journal_send(txid: &str) -> Activity {
    Activity {
        id: "send-1".to_owned(),
        kind: "send".to_owned(),
        from_account: Some(1),
        to_account: Some(2),
        to_address: None,
        source_pool: "ironwood".to_owned(),
        destination_pool: "ironwood".to_owned(),
        amount_zatoshi: 100_000_000,
        txid: txid.to_owned(),
        block_hash: Some("block".to_owned()),
        status: "confirmed".to_owned(),
        created_at: "2026-10-15 10:00:00".to_owned(),
        last_error: None,
    }
}

fn canonical_activity(direction: &'static str, txid: &str) -> CanonicalActivity {
    CanonicalActivity {
        direction,
        from_account: Some(1),
        to_account: Some(2),
        from_accounts: vec![1],
        to_accounts: vec![2],
        recipient_addresses: Vec::new(),
        source_pools: vec!["ironwood".to_owned()],
        destination_pools: vec!["ironwood".to_owned()],
        amount_zatoshi: 100_000_000,
        received_zatoshi: 100_000_000,
        spent_zatoshi: 0,
        net_zatoshi: 100_000_000,
        txid: txid.to_owned(),
        block_hash: Some("block".to_owned()),
        mined_height: Some(120),
        tx_index: Some(0),
        confirmations: 2,
        created_at: Some("2026-10-15 10:00:00".to_owned()),
        memos: Vec::new(),
    }
}

fn transparent_receive(receiver: &str) -> Transaction {
    let Address::Transparent(receiver) = Address::decode(&regtest_network(), receiver).unwrap()
    else {
        panic!("transparent receiver required")
    };
    TransactionData::<Authorized>::from_parts(
        TxVersion::V5,
        BranchId::for_height(&regtest_network(), BlockHeight::from_u32(2)),
        0,
        BlockHeight::from_u32(200),
        Some(Bundle {
            vin: vec![TxIn::from_parts(
                OutPoint::new([9; 32], 0),
                Script(script::Code(vec![1, 2])),
                u32::MAX,
            )],
            vout: vec![TxOut::new(
                Zatoshis::from_u64(100_000_000).unwrap(),
                Script(script::Code(receiver.script().to_bytes())),
            )],
            authorization: TransparentAuthorized,
        }),
        None,
        None,
        None,
    )
    .freeze()
    .unwrap()
}

#[test]
fn internal_transfer_is_returned_once_with_both_accounts() {
    let mut canonical = canonical_activity("outgoing", "internal-tx");
    canonical.to_account = None;
    canonical.to_accounts.clear();
    canonical.recipient_addresses = vec!["uregtest1account2".to_owned()];
    let activity = unified_activity(
        vec![journal_send("internal-tx")],
        vec![canonical.clone(), canonical],
        30,
    );

    assert_eq!(activity.len(), 1);
    assert_eq!(activity[0].id, "send-1");
    assert_eq!(activity[0].kind, "send");
    assert_eq!(activity[0].direction, "internal");
    assert_eq!(activity[0].from_account, Some(1));
    assert_eq!(activity[0].to_account, Some(2));
    assert_eq!(activity[0].from_accounts, [1]);
    assert_eq!(activity[0].to_accounts, [2]);
    assert_eq!(activity[0].to_address, None);
    assert_eq!(activity[0].recipient_addresses, ["uregtest1account2"]);
}

#[test]
fn multi_account_transaction_preserves_every_affected_account() {
    let mut canonical = canonical_activity("incoming", "multi-account-tx");
    canonical.from_account = None;
    canonical.to_account = None;
    canonical.from_accounts.clear();
    canonical.to_accounts = vec![2, 3];
    canonical.recipient_addresses = vec![
        "uregtest1account2".to_owned(),
        "uregtest1account3".to_owned(),
    ];
    canonical.amount_zatoshi = 300_000_000;

    let activity = unified_activity(Vec::new(), vec![canonical], 30);

    assert_eq!(activity.len(), 1);
    assert_eq!(activity[0].direction, "incoming");
    assert_eq!(activity[0].to_account, None);
    assert_eq!(activity[0].to_accounts, [2, 3]);
    assert_eq!(activity[0].to_address, None);
    assert_eq!(activity[0].amount_zatoshi, 300_000_000);
}

#[test]
fn canonical_confirmation_clears_a_stale_journal_error() {
    let mut journal = journal_send("confirmed-tx");
    journal.status = "broadcast".to_owned();
    journal.block_hash = None;
    journal.last_error = Some("auto-mine failed".to_owned());

    let activity = unified_activity(
        vec![journal],
        vec![canonical_activity("internal", "confirmed-tx")],
        30,
    );

    assert_eq!(activity.len(), 1);
    assert_eq!(activity[0].status, "confirmed");
    assert_eq!(activity[0].block_hash.as_deref(), Some("block"));
    assert_eq!(activity[0].last_error, None);
}

#[test]
fn external_incoming_and_unresolved_journal_entries_are_both_retained() {
    let mut incoming = canonical_activity("incoming", "external-tx");
    incoming.from_account = None;
    incoming.from_accounts.clear();
    incoming.to_account = Some(3);
    incoming.to_accounts = vec![3];
    incoming.recipient_addresses = vec!["uregtest1account3".to_owned()];
    let mut pending = journal_send("");
    pending.status = "preparing".to_owned();
    pending.last_error = Some("wallet is syncing".to_owned());

    let activity = unified_activity(vec![pending], vec![incoming], 30);

    assert_eq!(activity.len(), 2);
    assert!(activity.iter().any(|entry| {
        entry.to_account == Some(3)
            && entry.direction == "incoming"
            && entry.txid == "external-tx"
            && entry.recipient_addresses == ["uregtest1account3"]
    }));
    assert!(activity.iter().any(|entry| {
        entry.id == "send-1"
            && entry.txid.is_empty()
            && entry.received_zatoshi.is_none()
            && entry.spent_zatoshi.is_none()
            && entry.net_zatoshi.is_none()
            && entry.last_error.as_deref() == Some("wallet is syncing")
    }));
}

#[test]
fn canonical_activity_exposes_aggregate_wallet_value_effects() {
    let mut canonical = canonical_activity("outgoing", "value-effects");
    canonical.received_zatoshi = 90_000_000;
    canonical.spent_zatoshi = 200_000_000;
    canonical.net_zatoshi = -110_000_000;

    let activity = unified_activity(Vec::new(), vec![canonical], 30);

    assert_eq!(activity[0].received_zatoshi, Some(90_000_000));
    assert_eq!(activity[0].spent_zatoshi, Some(200_000_000));
    assert_eq!(activity[0].net_zatoshi, Some(-110_000_000));
}

#[tokio::test]
async fn activity_router_deduplicates_preparation_completed_after_its_journal_snapshot() {
    let (state, _dir) = state_with_local_wallet();
    let stale = state
        .0
        .store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "transparent",
            100_000_000,
            "snapshot-boundary",
            None,
        )
        .unwrap();
    let transaction = transparent_receive(&state.0.store.account(2).unwrap().transparent_address);
    let txid = transaction.txid();
    let mut raw = Vec::new();
    transaction.write(&mut raw).unwrap();
    state
        .0
        .wallet
        .enhance_transaction(txid, raw.clone(), 2)
        .await
        .unwrap();
    state
        .0
        .wallet
        .record_prepared_association_for_test(
            &stale.id,
            PreparedPayment {
                txid: txid.to_string(),
                raw_transaction: raw,
                expiry_height: 200,
            },
        )
        .await
        .unwrap();

    let (status, activity) = api_get(&state, "/api/v1/activity?limit=30").await;

    assert_eq!(status, StatusCode::OK);
    let matching = activity
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["txid"] == txid.to_string())
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0]["id"], stale.id);
    assert_eq!(matching[0]["received_zatoshi"], 100_000_000);
    assert_eq!(matching[0]["spent_zatoshi"], 0);
    assert_eq!(matching[0]["net_zatoshi"], 100_000_000);
}

#[test]
fn canonical_activity_preserves_every_recipient_address() {
    let mut canonical = canonical_activity("outgoing", "multi-recipient-tx");
    canonical.to_account = None;
    canonical.to_accounts.clear();
    canonical.recipient_addresses = vec![
        "uregtest1external1".to_owned(),
        "uregtest1external2".to_owned(),
    ];

    let activity = unified_activity(Vec::new(), vec![canonical], 30);

    assert_eq!(activity.len(), 1);
    assert_eq!(
        activity[0].recipient_addresses,
        ["uregtest1external1", "uregtest1external2"]
    );
    assert_eq!(
        activity[0].to_address.as_deref(),
        Some("uregtest1external1")
    );
}

#[test]
fn transparent_inputs_are_reported_as_a_public_source_pool() {
    let mut incoming = canonical_activity("incoming", "transparent-incoming");
    incoming.from_account = None;
    incoming.from_accounts.clear();
    incoming.source_pools.clear();

    add_public_source_pools(
        &mut incoming,
        &serde_json::json!({
            "vin": [{"txid": "previous-transaction", "vout": 0}]
        }),
    );

    let activity = unified_activity(Vec::new(), vec![incoming], 30);
    assert_eq!(activity[0].source_pool.as_deref(), Some("transparent"));
    assert_eq!(activity[0].source_pools, ["transparent"]);
}

#[test]
fn coinbase_does_not_look_like_a_transparent_payment_source() {
    let mut incoming = canonical_activity("incoming", "coinbase");
    incoming.from_account = None;
    incoming.from_accounts.clear();
    incoming.source_pools.clear();

    add_public_source_pools(
        &mut incoming,
        &serde_json::json!({"vin": [{"coinbase": "00"}]}),
    );

    let activity = unified_activity(Vec::new(), vec![incoming], 30);
    assert_eq!(activity[0].source_pool, None);
    assert!(activity[0].source_pools.is_empty());
}

#[test]
fn confirmed_activity_uses_chain_order_and_does_not_invent_incoming_creation_time() {
    let mut earlier = canonical_activity("internal", "earlier-tx");
    earlier.mined_height = Some(120);
    earlier.tx_index = Some(3);
    earlier.created_at = Some("2099-01-01 00:00:00".to_owned());

    let mut incoming = canonical_activity("incoming", "later-incoming-tx");
    incoming.from_account = None;
    incoming.from_accounts.clear();
    incoming.mined_height = Some(121);
    incoming.tx_index = Some(0);
    incoming.created_at = None;

    let activity = unified_activity(Vec::new(), vec![earlier, incoming], 30);

    assert_eq!(activity[0].txid, "later-incoming-tx");
    assert_eq!(activity[0].created_at, None);
    assert_eq!(activity[1].txid, "earlier-tx");
}

#[test]
fn confirmed_journal_outside_the_canonical_window_does_not_displace_recent_history() {
    let canonical = (101..=200)
        .map(|height| {
            let mut activity = canonical_activity("incoming", &format!("canonical-{height}"));
            activity.mined_height = Some(height);
            activity
        })
        .collect::<Vec<_>>();
    let old_confirmed = journal_send("old-confirmed");

    let activity = unified_activity(vec![old_confirmed], canonical, 1);

    assert_eq!(activity.len(), 1);
    assert_eq!(activity[0].txid, "canonical-200");
    assert_eq!(activity[0].mined_height, Some(200));
}

#[tokio::test]
async fn stalled_node_uses_one_bounded_enrichment_budget() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc = NodeRpc::new(format!("http://{}", listener.local_addr().unwrap()));
    let mut incoming = (0..16)
        .map(|index| {
            let mut activity = canonical_activity("incoming", &format!("incoming-{index}"));
            activity.from_account = None;
            activity.from_accounts.clear();
            activity.source_pools.clear();
            activity
        })
        .collect::<Vec<_>>();

    tokio::time::timeout(
        Duration::from_millis(250),
        enrich_external_source_pools_within(&rpc, &mut incoming, Duration::from_millis(50)),
    )
    .await
    .expect("incoming lookups exceeded their shared budget");
    assert!(
        incoming
            .iter()
            .all(|activity| activity.source_pools.is_empty())
    );
}

#[tokio::test]
async fn status_counts_accounts_without_reading_the_seed() {
    let (state, dir) = state_with_local_wallet();
    rusqlite::Connection::open(dir.path().join("server.db"))
        .unwrap()
        .execute("UPDATE metadata SET value='not-hex' WHERE key='seed'", [])
        .unwrap();

    let response = router(state)
        .oneshot(
            Request::get("/api/v1/status")
                .header("host", "127.0.0.1:8080")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let status: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status["account_count"], 5);
}

#[tokio::test]
async fn non_loopback_host_cannot_access_the_seed_endpoint() {
    let (state, _dir) = state_with_local_wallet();
    let response = router(state)
        .oneshot(
            Request::post("/api/v1/dev/seed")
                .header("host", "attacker.example")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"confirmation":"I understand this seed is for regtest only"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(response.headers()["content-type"], "application/json");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        error,
        serde_json::json!({
            "error": {
                "message": "Host must be a loopback address",
                "status": 403
            }
        })
    );
}

#[tokio::test]
async fn status_reports_the_selected_mining_mode() {
    for mode in [
        MiningMode::Auto,
        MiningMode::Interval,
        MiningMode::Mempool,
        MiningMode::Manual,
    ] {
        let (state, _dir) = state_with_mode(mode);
        let (code, status) = api_get(&state, "/api/v1/status").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(status["mining_mode"], json!(mode.as_str()));
        assert_eq!(status["auto_mine"], json!(mode.auto_mine()));
        assert_eq!(
            status["mine_on_mempool"],
            json!(matches!(mode, MiningMode::Mempool))
        );
        let block_time = if matches!(mode, MiningMode::Interval) {
            json!(2)
        } else {
            Value::Null
        };
        assert_eq!(status["block_time"], block_time, "{mode:?}");
    }
}
use crate::wallet::WALLET_BIRTHDAY_HEIGHT;

fn host(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(HOST, value.parse().unwrap());
    headers
}

#[test]
fn public_dashboard_endpoint_uses_configured_port_or_loopback_default() {
    assert_eq!(
        public_dashboard_endpoint(Some("http://127.0.0.1:32805"), &host("127.0.0.1:41234")),
        "http://127.0.0.1:32805"
    );
    assert_eq!(
        public_dashboard_endpoint(None, &HeaderMap::new()),
        "http://127.0.0.1:8080"
    );
    assert_eq!(
        public_dashboard_endpoint(None, &host("127.0.0.1:41234")),
        "http://127.0.0.1:8080"
    );
}

#[test]
fn public_dashboard_endpoint_reports_the_reached_port_when_run_assigned_none() {
    let unassigned = Some("http://127.0.0.1:0");
    assert_eq!(
        public_dashboard_endpoint(unassigned, &host("127.0.0.1:41234")),
        "http://127.0.0.1:41234"
    );
    assert_eq!(
        public_dashboard_endpoint(unassigned, &host("localhost:41234")),
        "http://localhost:41234"
    );
    assert_eq!(
        public_dashboard_endpoint(unassigned, &host("[::1]:41234")),
        "http://[::1]:41234"
    );
    // Without a usable Host (an HTTP/1.0 client), keep the configured value.
    assert_eq!(
        public_dashboard_endpoint(unassigned, &HeaderMap::new()),
        "http://127.0.0.1:0"
    );
    assert_eq!(
        public_dashboard_endpoint(unassigned, &host("not a host")),
        "http://127.0.0.1:0"
    );
    // Only the host and port are reported, never other authority parts.
    assert_eq!(
        public_dashboard_endpoint(unassigned, &host("user@127.0.0.1:41234")),
        "http://127.0.0.1:41234"
    );
}

fn wallet_status(
    state: &'static str,
    fully_scanned_height: Option<u64>,
    error: Option<&str>,
) -> WalletSyncStatus {
    WalletSyncStatus {
        state,
        fully_scanned_height,
        fully_scanned_hash: fully_scanned_height.map(|_| "canonical".to_owned()),
        observed_hash: fully_scanned_height.map(|_| "canonical".to_owned()),
        observed_height: fully_scanned_height,
        last_success_at: Some(123),
        error: error.map(str::to_owned),
    }
}

fn settlement_for_test(
    node: Option<u64>,
    lightwalletd: Option<u64>,
    wallet: &WalletSyncStatus,
) -> SettlementStatus {
    let checkpoint = |height| ChainCheckpoint {
        height,
        hash: "canonical".to_owned(),
    };
    settlement_status(
        node.map(checkpoint).as_ref(),
        lightwalletd.map(checkpoint).as_ref(),
        wallet,
        true,
    )
}

#[test]
fn settlement_requires_all_three_components_at_the_same_tip() {
    let status = settlement_for_test(
        Some(105),
        Some(105),
        &wallet_status("ready", Some(105), None),
    );
    assert!(status.settled);
    assert_eq!(status.settlement_blocker, None);

    let status = settlement_for_test(
        Some(105),
        Some(104),
        &wallet_status("ready", Some(105), None),
    );
    assert!(!status.settled);
    assert_eq!(status.settlement_blocker, Some("height_mismatch"));

    let status = settlement_for_test(
        Some(105),
        Some(105),
        &wallet_status("ready", Some(106), None),
    );
    assert!(
        !status.settled,
        "a wallet ahead of the current tip needs reconciliation"
    );

    let canonical = ChainCheckpoint {
        height: 105,
        hash: "canonical".to_owned(),
    };
    let replacement = ChainCheckpoint {
        height: 105,
        hash: "replacement".to_owned(),
    };
    let wallet = wallet_status("ready", Some(105), None);
    let status = settlement_status(Some(&canonical), Some(&replacement), &wallet, true);
    assert_eq!(status.settlement_blocker, Some("chain_mismatch"));

    let status = settlement_status(Some(&replacement), Some(&replacement), &wallet, true);
    assert!(
        !status.settled,
        "a same-height reorg needs wallet reconciliation"
    );
    assert_eq!(status.settlement_blocker, Some("wallet_behind"));
}

#[test]
fn settlement_reports_the_first_blocking_component() {
    let cases = [
        (
            None,
            Some(105),
            wallet_status("ready", Some(105), None),
            "node_unavailable",
        ),
        (
            Some(105),
            None,
            wallet_status("ready", Some(105), None),
            "lightwalletd_unavailable",
        ),
        (
            Some(105),
            Some(105),
            wallet_status("error", Some(104), Some("scan failed")),
            "wallet_error",
        ),
        (
            Some(105),
            Some(105),
            wallet_status("syncing", Some(104), None),
            "wallet_syncing",
        ),
        (
            Some(105),
            Some(105),
            wallet_status("ready", None, None),
            "wallet_behind",
        ),
        (
            Some(105),
            Some(105),
            wallet_status("ready", Some(104), None),
            "wallet_behind",
        ),
    ];

    for (node, lightwalletd, wallet, expected) in cases {
        let status = settlement_for_test(node, lightwalletd, &wallet);
        assert!(!status.settled);
        assert_eq!(status.settlement_blocker, Some(expected));
    }
}

#[test]
fn settlement_allows_a_canonical_tip_before_the_wallet_birthday() {
    let tip = ChainCheckpoint {
        height: u64::from(WALLET_BIRTHDAY_HEIGHT) - 1,
        hash: "canonical".to_owned(),
    };
    let mut wallet = wallet_status("ready", None, None);
    wallet.observed_height = Some(tip.height);
    wallet.observed_hash = Some(tip.hash.clone());
    assert!(settlement_status(Some(&tip), Some(&tip), &wallet, true).settled);
    wallet.observed_hash = Some("orphaned".to_owned());
    assert!(!settlement_status(Some(&tip), Some(&tip), &wallet, true).settled);
}

#[test]
fn settlement_waits_for_startup_after_the_components_agree() {
    let tip = ChainCheckpoint {
        height: 101,
        hash: "canonical".to_owned(),
    };
    let wallet = wallet_status("ready", Some(101), None);
    let status = settlement_status(Some(&tip), Some(&tip), &wallet, false);
    assert!(!status.settled);
    assert_eq!(status.settlement_blocker, Some("starting"));

    let status = settlement_status(None, Some(&tip), &wallet, false);
    assert_eq!(status.settlement_blocker, Some("node_unavailable"));

    assert!(settlement_status(Some(&tip), Some(&tip), &wallet, true).settled);
}

use crate::state::tests::{api_get, counting_rpc};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn lightwalletd_probe(
    listener: tokio::net::TcpListener,
) -> (String, Arc<AtomicBool>, tokio::task::JoinHandle<()>) {
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let available = Arc::new(AtomicBool::new(true));
    let availability = available.clone();
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (socket, _) = tokio::select! {
                socket = listener.accept() => socket.unwrap(),
                completed = connections.join_next(), if !connections.is_empty() => {
                    completed.unwrap().unwrap();
                    continue;
                }
            };
            let available = availability.clone();
            connections.spawn(async move {
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
                    if available.load(Ordering::SeqCst) {
                        // gRPC frame containing BlockID: height=1, hash=[1; 32].
                        let mut frame = vec![0, 0, 0, 0, 36, 0x08, 1, 0x12, 32];
                        frame.extend_from_slice(&[1; 32]);
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
    (endpoint, available, server)
}

#[tokio::test]
async fn health_checks_current_dependencies_without_requiring_settlement() {
    let (local, _dir) = state_with_local_wallet();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (lightwalletd, available, server) = lightwalletd_probe(listener);
    let node_available = Arc::new(AtomicBool::new(true));
    let availability = node_available.clone();
    let rpc = axum::Router::new().route(
        "/",
        axum::routing::post(move |Json(request): Json<Value>| {
            let available = availability.clone();
            async move {
                if !available.load(Ordering::SeqCst) {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                assert_eq!(request["method"], "getblockchaininfo");
                Json(json!({"result": {"chain": "regtest", "blocks": 101,
                    "bestblockhash": "a".repeat(64)}, "error": null, "id": request["id"]}))
                .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc_url = format!("http://{}", listener.local_addr().unwrap());
    let node = tokio::spawn(async move { axum::serve(listener, rpc).await.unwrap() });
    let state = AppState::new(
        local.0.store.clone(),
        local.0.wallet.clone().with_lightwalletd(lightwalletd),
        rpc_url,
        "test".into(),
    );
    state.mark_started();
    assert_eq!(
        api_get(&state, "/api/v1/health").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    state.0.wallet_snapshot.write().await.status.last_success_at = Some(1);
    for (node_up, lightwalletd_up) in [
        (true, true),
        (true, false),
        (true, true),
        (false, true),
        (true, true),
    ] {
        node_available.store(node_up, Ordering::SeqCst);
        available.store(lightwalletd_up, Ordering::SeqCst);
        let (code, health) = api_get(&state, "/api/v1/health").await;
        let ok = node_up && lightwalletd_up;
        assert_eq!(
            code,
            if ok {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            }
        );
        assert_eq!(health["ok"], ok);
        assert_eq!(health["wallet_sync"]["last_success_at"], 1);
        assert_eq!(health["wallet_sync"]["state"], "syncing");
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    let (code, health) = api_get(&state, "/api/v1/health").await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(health["ok"], false);
    let listener = tokio::net::TcpListener::bind(address).await.unwrap();
    let (_, _, server) = lightwalletd_probe(listener);
    let (code, health) = api_get(&state, "/api/v1/health").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(health["ok"], true);
    node.abort();
    assert!(node.await.unwrap_err().is_cancelled());
    let (code, health) = api_get(&state, "/api/v1/health").await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(health["ok"], false);
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn health_returns_unavailable_when_lightwalletd_stalls() {
    let (local, _dir) = state_with_local_wallet();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (rpc_url, _) = counting_rpc(true).await;
    let state = AppState::new(
        local.0.store.clone(),
        local.0.wallet.clone().with_lightwalletd(endpoint),
        rpc_url,
        "test".into(),
    );
    state.mark_started();
    state.0.wallet_snapshot.write().await.status.last_success_at = Some(1);
    // The bound listener never answers the HTTP/2 handshake.
    let (code, health) = tokio::time::timeout(
        std::time::Duration::from_secs(6),
        api_get(&state, "/api/v1/health"),
    )
    .await
    .expect("health ignored the lightwalletd probe deadline");
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(health["ok"], false);
}

#[tokio::test]
async fn activity_lookup_by_key_returns_every_state_regardless_of_list_limits() {
    let (state, _dir) = state_with_local_wallet();
    let store = &state.0.store;
    let preparing = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            1_000,
            "lookup-preparing",
            Some(""),
        )
        .unwrap();
    let pending = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "transparent",
            2_000,
            "lookup-pending",
            None,
        )
        .unwrap();
    store
        .record_prepared(&pending.id, "pending-txid", b"raw", 140)
        .unwrap();
    store.mark_broadcast(&pending.id, "pending-txid").unwrap();
    let failed = store
        .claim_transfer(
            2,
            crate::db::Recipient::Account(1),
            "ironwood",
            "ironwood",
            3_000,
            "lookup-failed",
            None,
        )
        .unwrap();
    store.fail(&failed.id, "insufficient funds").unwrap();
    let confirmed = store
        .claim_faucet(3, "ironwood", 4_000, "lookup-confirmed")
        .unwrap();
    store
        .record_prepared(&confirmed.id, "confirmed-txid", b"raw", 140)
        .unwrap();
    store
        .confirm(&confirmed.id, "confirmed-txid", "block")
        .unwrap();
    store
        .claim_address_faucet("tm-external", 5, "lookup-external")
        .unwrap();

    let (status, listed) = api_get(&state, "/api/v1/activity?limit=10").await;
    assert_eq!(status, StatusCode::OK);
    let listed: Vec<_> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["id"].clone(),
                row["status"].clone(),
                row["txid"].clone(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            (json!(failed.id), json!("failed"), json!("")),
            (json!(pending.id), json!("broadcast"), json!("pending-txid")),
            (json!(preparing.id), json!("preparing"), json!("")),
            (
                json!(confirmed.id),
                json!("confirmed"),
                json!("confirmed-txid")
            ),
        ]
    );

    for index in 0..101 {
        store
            .claim_transfer(
                1,
                crate::db::Recipient::Account(3),
                "ironwood",
                "ironwood",
                1,
                &format!("filler-{index:03}"),
                None,
            )
            .unwrap();
    }
    let (_, listed) = api_get(&state, "/api/v1/activity?limit=100").await;
    assert_eq!(listed.as_array().unwrap().len(), 100);
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["id"] != json!(preparing.id))
    );

    for (key, id, status, txid, last_error) in [
        (
            "lookup-preparing",
            &preparing.id,
            "preparing",
            "",
            Value::Null,
        ),
        (
            "lookup-pending",
            &pending.id,
            "broadcast",
            "pending-txid",
            Value::Null,
        ),
        (
            "lookup-failed",
            &failed.id,
            "failed",
            "",
            json!("insufficient funds"),
        ),
        (
            "lookup-confirmed",
            &confirmed.id,
            "confirmed",
            "confirmed-txid",
            Value::Null,
        ),
    ] {
        let (code, found) = api_get(
            &state,
            &format!("/api/v1/activity?limit=1&idempotency_key={key}"),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{key}");
        let found = found.as_array().unwrap();
        assert_eq!(found.len(), 1, "{key}");
        assert_eq!(found[0]["id"], json!(id), "{key}");
        assert_eq!(found[0]["status"], status, "{key}");
        assert_eq!(found[0]["txid"], txid, "{key}");
        assert_eq!(found[0]["last_error"], last_error, "{key}");
    }
    for key in ["lookup-missing", "lookup-external"] {
        assert_eq!(
            api_get(&state, &format!("/api/v1/activity?idempotency_key={key}")).await,
            (StatusCode::OK, json!([]))
        );
    }
    let (code, _) = api_get(&state, "/api/v1/activity?idempotency_key=").await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn stored_status_reads_have_no_wallet_or_node_side_effects() {
    let (state, dir) = state_with_local_wallet();
    let (rpc_url, calls) = counting_rpc(true).await;
    let state = AppState::new(
        state.0.store.clone(),
        state.0.wallet.clone(),
        rpc_url,
        "test".into(),
    );
    let store = &state.0.store;
    let send = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            1_000,
            "read-send",
            None,
        )
        .unwrap();
    store
        .claim_faucet(2, "transparent", 2_000, "read-internal")
        .unwrap();
    let external = store
        .claim_address_faucet("tm-external", 3_000, "read-external")
        .unwrap();
    let activities = store.unresolved_activities().unwrap();
    let faucets = store.unresolved_address_faucets().unwrap();

    for uri in [
        "/api/v1/activity",
        "/api/v1/activity?idempotency_key=read-send",
        "/api/v1/faucet/address?idempotency_key=read-internal",
        "/api/v1/faucet/address?idempotency_key=read-external",
        "/api/v1/faucet/address?idempotency_key=read-missing",
    ] {
        api_get(&state, uri).await;
    }

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(store.unresolved_activities().unwrap(), activities);
    assert_eq!(store.unresolved_address_faucets().unwrap(), faucets);
    for id in [&send.id, &external.id] {
        assert!(store.prepared_transaction(id).is_err());
        assert!(state.0.wallet.prepared(id).await.unwrap().is_none());
    }
    drop(dir);
}

use crate::state::tests::api_request;
#[tokio::test]
async fn health_and_chain_effects_wait_until_startup_finishes() {
    let (state, _dir) = state_with_local_wallet();
    let (rpc_url, calls) = counting_rpc(true).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (lightwalletd, _, server) = lightwalletd_probe(listener);
    let state = AppState::new(
        state.0.store.clone(),
        state.0.wallet.clone().with_lightwalletd(lightwalletd),
        rpc_url,
        "test".into(),
    );
    state.0.wallet_snapshot.write().await.status.last_success_at = Some(1);
    let account = state.0.store.account(2).unwrap();

    let (code, health) = api_get(&state, "/api/v1/health").await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(health["ok"], false);
    assert_eq!(health["node"]["blocks"], 101);
    let health_calls = calls.load(Ordering::SeqCst);

    for (uri, body) in [
        ("/api/v1/send", valid_send()),
        (
            "/api/v1/faucet",
            json!({"account_id": 2, "pool": "ironwood", "amount_zatoshi": 1_000, "idempotency_key": "startup-faucet"}),
        ),
        (
            "/api/v1/faucet/address",
            json!({"address": account.transparent_address, "amount_zatoshi": 1_000, "idempotency_key": "startup-address"}),
        ),
        ("/api/v1/mine", json!({"blocks": 1})),
        (
            "/api/v1/mining/jobs",
            json!({"blocks": 1, "idempotency_key": "startup-mining"}),
        ),
    ] {
        let (code, body) = api_request(&state, "POST", uri, body).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(
            body["error"]["message"],
            "the environment is still starting"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), health_calls);
    assert!(state.0.store.activities(100).unwrap().is_empty());
    assert!(
        state
            .0
            .store
            .unresolved_address_faucets()
            .unwrap()
            .is_empty()
    );
    assert!(state.0.mining.latest().await.is_none());

    state.mark_started();
    let (code, health) = api_get(&state, "/api/v1/health").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(health["ok"], true);
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

use crate::api::payments::tests::valid_send;
