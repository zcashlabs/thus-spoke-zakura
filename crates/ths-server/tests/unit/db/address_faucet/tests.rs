use super::*;
use std::sync::Barrier;

fn order_rows(store: &Store, id: &str) -> usize {
    store
        .0
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM activity_order WHERE activity_id=?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

fn order_count(store: &Store) -> usize {
    store
        .0
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM activity_order", [], |row| row.get(0))
        .unwrap()
}

fn stored_sequence(store: &Store, id: &str) -> i64 {
    store
        .0
        .lock()
        .unwrap()
        .query_row(
            "SELECT sequence FROM activity_order WHERE activity_id=?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn address_faucet_replay_survives_restart_and_rejects_conflicting_intents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.db");
    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    let address = store.account(1).unwrap().unified_address;
    let first = store
        .claim_address_faucet(&address, 100_000_000, "address-key")
        .unwrap();
    let first = store
        .record_address_prepared(&first.id, "txid-one", b"signed bytes", 140)
        .unwrap();
    assert_eq!(first.status, "prepared");
    drop(store);

    let reopened = Store::open(&path).unwrap();
    reopened.initialize().unwrap();
    let replay = reopened
        .claim_address_faucet(&address, 100_000_000, "address-key")
        .unwrap();
    assert_eq!(replay.id, first.id);
    assert_eq!(replay.txid, "txid-one");
    assert_eq!(
        reopened
            .prepared_transaction(&replay.id)
            .unwrap()
            .raw_transaction,
        b"signed bytes"
    );
    assert!(
        reopened
            .claim_address_faucet(&address, 2, "address-key")
            .is_err()
    );
    assert!(
        reopened
            .claim_address_faucet(
                &reopened.account(2).unwrap().unified_address,
                100_000_000,
                "address-key"
            )
            .is_err()
    );
    assert!(
        reopened
            .claim_faucet(1, "ironwood", 100_000_000, "address-key")
            .is_err()
    );
    assert!(
        reopened
            .claim_address_faucet(&address, 100_000_000, "fresh-key")
            .is_ok()
    );
    reopened
        .claim_faucet(1, "ironwood", 100_000_000, "account-key")
        .unwrap();
    assert!(
        reopened
            .claim_address_faucet(&address, 100_000_000, "account-key")
            .is_err()
    );
}

#[test]
fn address_faucet_claim_records_first_seen_order_before_any_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.db");
    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();

    let external = store
        .claim_address_faucet("tm-external-order", 1_000, "external-order")
        .unwrap();
    assert_eq!(order_rows(&store, &external.id), 1);
    let external_sequence = stored_sequence(&store, &external.id);

    let account = store
        .claim_faucet(2, "ironwood", 2_000, "account-order")
        .unwrap();
    assert!(stored_sequence(&store, &account.id) > external_sequence);

    let replay = store
        .claim_address_faucet("tm-external-order", 1_000, "external-order")
        .unwrap();
    assert_eq!(replay.id, external.id);
    assert_eq!(stored_sequence(&store, &external.id), external_sequence);
    assert!(
        store
            .claim_address_faucet("tm-external-order", 2_000, "external-order")
            .is_err()
    );
    assert_eq!(stored_sequence(&store, &external.id), external_sequence);

    drop(store);
    let reopened = Store::open(&path).unwrap();
    reopened.initialize().unwrap();
    assert_eq!(order_rows(&reopened, &external.id), 1);
    assert_eq!(stored_sequence(&reopened, &external.id), external_sequence);
    assert!(stored_sequence(&reopened, &account.id) > external_sequence);
}

#[test]
fn address_faucet_claim_writes_payment_and_order_atomically() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let before = order_count(&store);
    store.0.lock().unwrap().execute_batch(
        "CREATE TRIGGER reject_order BEFORE INSERT ON activity_order BEGIN SELECT RAISE(ABORT,'injected failure'); END;"
    ).unwrap();

    assert!(
        store
            .claim_address_faucet("tm-atomic", 100, "atomic-order")
            .is_err()
    );
    assert!(
        store
            .address_faucet_for_key("atomic-order")
            .unwrap()
            .is_none()
    );
    assert_eq!(order_count(&store), before);
}

#[test]
fn concurrent_address_faucet_claims_share_one_operation() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let claims: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.claim_address_faucet("same-address", 1, "same-operation")
            })
        })
        .collect();
    barrier.wait();
    let ids: Vec<_> = claims
        .into_iter()
        .map(|claim| claim.join().unwrap().unwrap().id)
        .collect();
    assert_eq!(ids[0], ids[1]);
}

#[test]
fn address_faucet_errors_and_failures_never_overwrite_newer_state() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_address_faucet("tm-external", 100, "address-guarded")
        .unwrap();
    store
        .record_address_error(&claim.id, "preparing", "", "treasury is syncing")
        .unwrap();
    let prepared = store
        .record_address_prepared(&claim.id, "txid-1", b"raw", 140)
        .unwrap();
    assert_eq!(prepared.last_error, None);
    let stale = store
        .record_address_error(&claim.id, "preparing", "", "stale")
        .unwrap();
    assert_eq!(stale.last_error, None);
    assert!(store.fail_address(&claim.id, "stale").is_err());
    store
        .record_address_error(&claim.id, "prepared", "txid-1", "broadcast unavailable")
        .unwrap();
    assert_eq!(
        store
            .mark_address_broadcast(&claim.id, "txid-1")
            .unwrap()
            .last_error,
        None
    );
    store
        .record_address_error(&claim.id, "broadcast", "txid-1", "auto-mine failed")
        .unwrap();
    let confirmed = store.confirm_address(&claim.id, "txid-1", "block").unwrap();
    assert_eq!(confirmed.last_error, None);
    assert_eq!(
        store
            .record_address_error(&claim.id, "confirmed", "txid-1", "late")
            .unwrap()
            .last_error,
        None
    );
    assert!(store.fail_address(&claim.id, "late").is_err());

    let doomed = store
        .claim_address_faucet("tm-external", 100, "address-doomed")
        .unwrap();
    let failed = store
        .fail_address(&doomed.id, "treasury cannot pay")
        .unwrap();
    assert_eq!(failed.status, "failed");
    assert_eq!(
        store
            .fail_address(&doomed.id, "another reason")
            .unwrap()
            .last_error
            .as_deref(),
        Some("treasury cannot pay")
    );
    store
        .record_address_error(&doomed.id, "failed", "", "overwrite")
        .unwrap();
    assert!(
        store
            .record_address_prepared(&doomed.id, "late-txid", b"late", 140)
            .is_err()
    );
    store.discard_address_preparing(&doomed.id).unwrap();
    let replay = store
        .claim_address_faucet("tm-external", 100, "address-doomed")
        .unwrap();
    assert_eq!(replay, failed);
}

#[test]
fn clearing_address_errors_requires_the_exact_pending_state_and_transaction() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    for status in ["preparing", "prepared", "broadcast"] {
        let claim = store
            .claim_address_faucet("external-address", 100, status)
            .unwrap();
        let txid = if status == "preparing" {
            ""
        } else {
            "current-txid"
        };
        if status != "preparing" {
            store
                .record_address_prepared(&claim.id, txid, b"signed bytes", 140)
                .unwrap();
        }
        if status == "broadcast" {
            store.mark_address_broadcast(&claim.id, txid).unwrap();
        }
        let saved = store
            .record_address_error(&claim.id, status, txid, "current error")
            .unwrap();
        let stale_status = if status == "preparing" {
            "prepared"
        } else {
            "preparing"
        };
        assert_eq!(
            store
                .clear_address_error(&claim.id, stale_status, txid)
                .unwrap(),
            saved
        );
        assert_eq!(
            store
                .clear_address_error(&claim.id, status, "stale-txid")
                .unwrap(),
            saved
        );
        let cleared = store.clear_address_error(&claim.id, status, txid).unwrap();
        assert_eq!(cleared.last_error, None);
        assert_eq!(
            (cleared.status.as_str(), cleared.txid.as_str()),
            (status, txid)
        );
        assert_eq!(
            store.clear_address_error(&claim.id, status, txid).unwrap(),
            cleared
        );
    }
    let failed = store
        .claim_address_faucet("external-address", 100, "failed-clear")
        .unwrap();
    let failed = store.fail_address(&failed.id, "terminal failure").unwrap();
    assert_eq!(
        store.clear_address_error(&failed.id, "failed", "").unwrap(),
        failed
    );
    assert_eq!(
        store
            .clear_address_error(&failed.id, "preparing", "")
            .unwrap(),
        failed
    );

    let confirmed = store
        .claim_address_faucet("external-address", 100, "confirmed-clear")
        .unwrap();
    store
        .record_address_prepared(&confirmed.id, "included-txid", b"signed bytes", 140)
        .unwrap();
    store
        .confirm_address(&confirmed.id, "included-txid", "included-block")
        .unwrap();
    store
        .0
        .lock()
        .unwrap()
        .execute(
            "UPDATE address_faucets SET last_error='retained terminal diagnostic' WHERE id=?1",
            [&confirmed.id],
        )
        .unwrap();
    let confirmed = store
        .address_faucet_for_key("confirmed-clear")
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .clear_address_error(&confirmed.id, "confirmed", "included-txid")
            .unwrap(),
        confirmed
    );
    assert_eq!(
        store
            .clear_address_error(&confirmed.id, "broadcast", "included-txid")
            .unwrap(),
        confirmed
    );
}
