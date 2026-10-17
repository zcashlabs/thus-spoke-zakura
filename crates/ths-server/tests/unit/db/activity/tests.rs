use super::*;
use crate::db::address_faucet::AddressFaucetIntent;
use std::sync::Barrier;

#[test]
fn unconfirming_blocks_preserves_internal_and_external_requests_and_signed_bytes() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let account = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "orphaned-send",
            Some("memo"),
        )
        .unwrap();
    store
        .record_prepared(&account.id, "send-tx", b"send bytes", 140)
        .unwrap();
    store.confirm(&account.id, "send-tx", "removed").unwrap();
    let external = store
        .claim_address_faucet("tm-external", 12_000, "orphaned-external")
        .unwrap();
    store
        .record_address_prepared(&external.id, "external-tx", b"external bytes", 140)
        .unwrap();
    store
        .confirm_address(&external.id, "external-tx", "removed")
        .unwrap();
    let retained = store
        .claim_faucet(3, "ironwood", 12_000, "retained-faucet")
        .unwrap();
    store
        .record_prepared(&retained.id, "retained-tx", b"retained bytes", 140)
        .unwrap();
    store
        .confirm(&retained.id, "retained-tx", "canonical")
        .unwrap();

    store.unconfirm_blocks(&["removed".into()]).unwrap();
    let orphan = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "orphaned-send",
            Some("memo"),
        )
        .unwrap();
    let faucet = store
        .address_faucet_for_key("orphaned-external")
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            orphan.id.as_str(),
            orphan.txid.as_str(),
            orphan.status.as_str()
        ),
        (account.id.as_str(), "send-tx", "unconfirmed")
    );
    assert_eq!(
        (
            faucet.id.as_str(),
            faucet.txid.as_str(),
            faucet.status.as_str()
        ),
        (external.id.as_str(), "external-tx", "unconfirmed")
    );
    assert_eq!(orphan.block_hash, None);
    assert_eq!(faucet.block_hash, None);
    assert_eq!(
        store
            .prepared_transaction(&account.id)
            .unwrap()
            .raw_transaction,
        b"send bytes"
    );
    assert_eq!(
        store
            .optional_prepared_transaction(&external.id)
            .unwrap()
            .unwrap()
            .raw_transaction,
        b"external bytes"
    );
    assert_eq!(
        store.activity(&retained.id).unwrap().block_hash.as_deref(),
        Some("canonical")
    );
    assert!(
        store
            .claim_transfer(
                1,
                Recipient::Account(2),
                "ironwood",
                "ironwood",
                12_001,
                "orphaned-send",
                Some("memo")
            )
            .is_err()
    );

    assert!(store.confirm_reorged_transaction("send-tx", "").is_err());
    store
        .confirm_reorged_transaction("send-tx", "replacement")
        .unwrap();
    store
        .confirm_reorged_transaction("external-tx", "replacement")
        .unwrap();
    assert_eq!(
        store.activity(&account.id).unwrap().block_hash.as_deref(),
        Some("replacement")
    );
    assert_eq!(
        store
            .address_faucet_for_key("orphaned-external")
            .unwrap()
            .unwrap()
            .status,
        "confirmed"
    );
}

#[test]
fn unconfirming_both_payment_collections_is_atomic() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let account = store
        .claim_faucet(2, "ironwood", 12_000, "atomic-unconfirm")
        .unwrap();
    store
        .record_prepared(&account.id, "tx", b"bytes", 140)
        .unwrap();
    store.confirm(&account.id, "tx", "removed").unwrap();
    let external = store
        .claim_address_faucet("tm-external", 12_000, "atomic-external")
        .unwrap();
    store
        .record_address_prepared(&external.id, "external", b"bytes", 140)
        .unwrap();
    store
        .confirm_address(&external.id, "external", "removed")
        .unwrap();
    store.0.lock().unwrap().execute_batch(
        "CREATE TRIGGER reject_unconfirm BEFORE UPDATE ON address_faucets BEGIN SELECT RAISE(ABORT,'injected failure'); END;"
    ).unwrap();
    assert!(store.unconfirm_blocks(&["removed".into()]).is_err());
    assert_eq!(store.activity(&account.id).unwrap().status, "confirmed");
    assert_eq!(
        store.activity(&account.id).unwrap().block_hash.as_deref(),
        Some("removed")
    );
}
#[test]
fn records_real_transfer_without_mutating_balances() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let activity = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "send",
            None,
        )
        .unwrap();
    assert_eq!(store.activities(10).unwrap().len(), 1);
    let activity = store
        .record_prepared(&activity.id, "real-txid", b"raw transaction", 140)
        .unwrap();
    let activity = store.mark_broadcast(&activity.id, &activity.txid).unwrap();
    assert_eq!(activity.txid, "real-txid");
    assert_eq!(store.account(1).unwrap().transparent_zatoshi, 0);
}

#[test]
fn unconfirmed_activities_skip_unprepared_and_confirmed_rows() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            11_000,
            "preparing-key",
            None,
        )
        .unwrap();
    let pending = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "pending-key",
            None,
        )
        .unwrap();
    let pending = store
        .record_prepared(&pending.id, "txid-pending", b"pending", 140)
        .unwrap();
    let pending = store.mark_broadcast(&pending.id, &pending.txid).unwrap();
    let mined = store
        .claim_transfer(
            1,
            Recipient::Account(3),
            "ironwood",
            "ironwood",
            13_000,
            "mined-key",
            None,
        )
        .unwrap();
    let mined = store
        .record_prepared(&mined.id, "txid-mined", b"mined", 140)
        .unwrap();
    let mined = store.mark_broadcast(&mined.id, &mined.txid).unwrap();
    store
        .confirm(&mined.id, &mined.txid, &"c".repeat(64))
        .unwrap();

    let open = store.unconfirmed_activities().unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, pending.id);
    assert_eq!(open[0].status, "broadcast");
    assert_eq!(open[0].txid, "txid-pending");
}

#[test]
fn confirm_rejects_an_empty_block_hash() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let pending = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "empty-hash",
            None,
        )
        .unwrap();
    let pending = store
        .record_prepared(&pending.id, "txid-empty", b"raw", 140)
        .unwrap();
    let pending = store.mark_broadcast(&pending.id, &pending.txid).unwrap();

    assert!(store.confirm(&pending.id, &pending.txid, "").is_err());
    let again = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "empty-hash",
            None,
        )
        .unwrap();
    assert_eq!(again.id, pending.id);
    assert_eq!(again.status, "broadcast");
    assert_eq!(again.block_hash, None);
}

#[test]
fn rejects_reusing_a_key_for_a_different_payment() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
        .unwrap();

    let error = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            13_000,
            "same",
            None,
        )
        .unwrap_err();

    assert!(error.to_string().contains("different payment"));
    assert!(error.downcast_ref::<IdempotencyConflict>().is_some());

    let error = store
        .claim_faucet(2, "ironwood", 12_000, "same")
        .unwrap_err();
    assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
}

#[test]
fn reusing_a_key_conflicts_on_every_payment_field() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "send",
            None,
        )
        .unwrap();
    store.claim_faucet(2, "ironwood", 12_000, "faucet").unwrap();

    for (from, to, source, destination, amount) in [
        (3, 2, "ironwood", "ironwood", 12_000),
        (1, 3, "ironwood", "ironwood", 12_000),
        (1, 2, "transparent", "ironwood", 12_000),
        (1, 2, "ironwood", "transparent", 12_000),
        (1, 2, "ironwood", "ironwood", 13_000),
    ] {
        let error = store
            .claim_transfer(
                from,
                Recipient::Account(to),
                source,
                destination,
                amount,
                "send",
                None,
            )
            .unwrap_err();
        assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
    }
    for (to, pool, amount) in [
        (3, "ironwood", 12_000),
        (2, "transparent", 12_000),
        (2, "ironwood", 13_000),
    ] {
        let error = store.claim_faucet(to, pool, amount, "faucet").unwrap_err();
        assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
    }
    let error = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "faucet",
            None,
        )
        .unwrap_err();
    assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
}

#[test]
fn send_and_faucet_validate_their_own_requests() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();

    for (from, to, source, destination, amount) in [
        (9, 2, "ironwood", "ironwood", 12_000),
        (1, 9, "ironwood", "ironwood", 12_000),
        (1, 2, "sapling", "ironwood", 12_000),
        (1, 2, "ironwood", "sapling", 12_000),
        (1, 2, "ironwood", "ironwood", 0),
    ] {
        assert!(
            store
                .claim_transfer(
                    from,
                    Recipient::Account(to),
                    source,
                    destination,
                    amount,
                    "send",
                    None
                )
                .is_err()
        );
    }
    for (to, pool, amount) in [
        (9, "ironwood", 12_000),
        (2, "sapling", 12_000),
        (2, "ironwood", 0),
    ] {
        assert!(store.claim_faucet(to, pool, amount, "faucet").is_err());
    }
    assert!(store.activity_for_key("send").unwrap().is_none());
    assert!(store.activity_for_key("faucet").unwrap().is_none());

    let faucet = store
        .claim_faucet(2, "transparent", 12_000, "faucet")
        .unwrap();
    assert_eq!(faucet.kind, "faucet");
    assert_eq!(faucet.from_account, None);
    assert_eq!(faucet.source_pool, "ironwood");
    assert_eq!(faucet.destination_pool, "transparent");
    assert_eq!(faucet.status, "preparing");
    assert!(faucet.txid.is_empty());
}

#[test]
fn memo_is_part_of_the_claimed_payment() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let first = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "memo-key",
            Some("rent"),
        )
        .unwrap();
    assert_eq!(
        store
            .claim_transfer(
                1,
                Recipient::Account(2),
                "ironwood",
                "ironwood",
                12_000,
                "memo-key",
                Some("rent")
            )
            .unwrap()
            .id,
        first.id
    );
    for memo in [None, Some(""), Some("gift")] {
        let error = store
            .claim_transfer(
                1,
                Recipient::Account(2),
                "ironwood",
                "ironwood",
                12_000,
                "memo-key",
                memo,
            )
            .unwrap_err();
        assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
    }
}

#[test]
fn concurrent_claims_create_one_payment() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let threads = (0..2)
        .map(|_| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .claim_transfer(
                        1,
                        Recipient::Account(2),
                        "ironwood",
                        "ironwood",
                        12_000,
                        "same",
                        None,
                    )
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let claims = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(claims[0].id, claims[1].id);
    assert_eq!(claims[0].status, "preparing");
    let listed = store.activities(10).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, claims[0].id);
    assert!(listed[0].txid.is_empty());
}

#[test]
fn discarding_a_failed_prepare_removes_its_claim() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let failed = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
        .unwrap();

    store.discard_preparing(&failed.id).unwrap();

    assert!(store.activity_for_key("same").unwrap().is_none());
    let retry = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
        .unwrap();
    assert_ne!(retry.id, failed.id);
}

#[test]
fn prepared_payment_can_be_reset_after_expiry() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
        .unwrap();
    store
        .record_prepared(&claim.id, "expired-txid", b"signed transaction", 140)
        .unwrap();

    let retry = store.reset_for_retry(&claim.id, "expired-txid").unwrap();

    assert_eq!(retry.status, "preparing");
    assert!(retry.txid.is_empty());
    assert!(store.prepared_transaction(&claim.id).is_err());
    assert_eq!(store.activities(10).unwrap(), [retry]);
}

#[test]
fn recording_the_same_prepared_transaction_converges() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
        .unwrap();
    let first = store
        .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
        .unwrap();

    let second = store
        .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
        .unwrap();

    assert_eq!(second.id, first.id);
    assert_eq!(second.txid, first.txid);
    assert_eq!(second.status, "prepared");
}

#[test]
fn recording_prepared_bytes_backfills_a_legacy_broadcast() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
        .unwrap();
    let prepared = store
        .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
        .unwrap();
    let broadcast = store.mark_broadcast(&prepared.id, &prepared.txid).unwrap();
    store
        .0
        .lock()
        .unwrap()
        .execute("DELETE FROM prepared_payments", [])
        .unwrap();

    store
        .record_prepared(
            &broadcast.id,
            &broadcast.txid,
            b"recovered transaction",
            140,
        )
        .unwrap();

    assert_eq!(
        store
            .prepared_transaction(&broadcast.id)
            .unwrap()
            .raw_transaction,
        b"recovered transaction"
    );
}

#[test]
fn stale_retry_does_not_reset_a_replacement_transaction() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
        .unwrap();
    store
        .record_prepared(&claim.id, "old-txid", b"old transaction", 140)
        .unwrap();
    store.reset_for_retry(&claim.id, "old-txid").unwrap();
    store
        .record_prepared(&claim.id, "new-txid", b"new transaction", 180)
        .unwrap();

    let current = store.reset_for_retry(&claim.id, "old-txid").unwrap();

    assert_eq!(current.txid, "new-txid");
    assert_eq!(current.status, "prepared");
}

#[test]
fn broadcast_reconciliation_preserves_a_concurrent_confirmation() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
        .unwrap();
    store
        .record_prepared(&claim.id, "real-txid", b"transaction", 140)
        .unwrap();
    store
        .confirm(&claim.id, "real-txid", "mined-block")
        .unwrap();

    let current = store.mark_broadcast(&claim.id, "real-txid").unwrap();

    assert_eq!(current.status, "confirmed");
    assert_eq!(current.block_hash.as_deref(), Some("mined-block"));
}

#[test]
fn prepared_transaction_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
        .unwrap();
    let id = claim.id.clone();
    store
        .record_prepared(&id, "real-txid", b"raw transaction", 140)
        .unwrap();
    drop(store);

    let reopened = Store::open(path).unwrap();
    reopened.initialize().unwrap();
    let recovered = reopened
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
        .unwrap();

    assert_eq!(recovered.txid, "real-txid");
    assert_eq!(recovered.status, "prepared");
    let prepared = reopened.prepared_transaction(&id).unwrap();
    assert_eq!(prepared.raw_transaction, b"raw transaction");
    assert_eq!(prepared.expiry_height, 140);
}

#[test]
fn records_a_send_to_an_external_address() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let activity = store
        .claim_transfer(
            1,
            Recipient::Address("tmExternal"),
            "ironwood",
            "transparent",
            12_000,
            "external-key",
            None,
        )
        .unwrap();
    assert_eq!(activity.to_account, None);
    assert_eq!(activity.to_address.as_deref(), Some("tmExternal"));

    let replayed = store
        .claim_transfer(
            1,
            Recipient::Address("tmExternal"),
            "ironwood",
            "transparent",
            12_000,
            "external-key",
            None,
        )
        .unwrap();
    assert_eq!(replayed.id, activity.id);

    // Reusing the key for a different recipient is a different payment.
    let conflict = store
        .claim_transfer(
            1,
            Recipient::Address("tmOther"),
            "ironwood",
            "transparent",
            12_000,
            "external-key",
            None,
        )
        .unwrap_err();
    assert!(conflict.downcast_ref::<IdempotencyConflict>().is_some());
}

#[test]
fn external_recipient_survives_recovery_and_shares_the_faucet_key_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    let claimed = store
        .claim_transfer(
            1,
            Recipient::Address("tmExternal"),
            "ironwood",
            "transparent",
            12_000,
            "external-send",
            None,
        )
        .unwrap();
    store
        .record_prepared(&claimed.id, "txid-external", b"signed transaction", 140)
        .unwrap();
    drop(store);

    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    let replayed = store.activity_for_key("external-send").unwrap().unwrap();
    assert_eq!(replayed.id, claimed.id);
    assert_eq!(replayed.to_account, None);
    assert_eq!(replayed.to_address.as_deref(), Some("tmExternal"));
    assert_eq!(store.unconfirmed_activities().unwrap()[0].id, claimed.id);
    assert_eq!(
        store
            .prepared_transaction(&claimed.id)
            .unwrap()
            .raw_transaction,
        b"signed transaction"
    );
    let broadcast = store.mark_broadcast(&claimed.id, "txid-external").unwrap();
    assert_eq!(broadcast.to_address.as_deref(), Some("tmExternal"));
    let confirmed = store
        .confirm(&claimed.id, "txid-external", "block")
        .unwrap();
    assert_eq!(confirmed.to_address.as_deref(), Some("tmExternal"));
    assert_eq!(store.activities(10).unwrap()[0].to_account, None);
    assert!(store.unconfirmed_activities().unwrap().is_empty());

    let conflict = store
        .claim_address_faucet("tmExternal", 12_000, "external-send")
        .unwrap_err();
    assert!(conflict.downcast_ref::<IdempotencyConflict>().is_some());
    store
        .claim_address_faucet("tmExternal", 12_000, "external-faucet")
        .unwrap();
    let conflict = store
        .claim_transfer(
            1,
            Recipient::Address("tmExternal"),
            "ironwood",
            "transparent",
            12_000,
            "external-faucet",
            None,
        )
        .unwrap_err();
    assert!(conflict.downcast_ref::<IdempotencyConflict>().is_some());
}

#[test]
fn payment_errors_and_failures_never_overwrite_newer_state() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "guarded",
            None,
        )
        .unwrap();
    let errored = store
        .record_error(&claim.id, "preparing", "", "wallet is syncing")
        .unwrap();
    assert_eq!(errored.last_error.as_deref(), Some("wallet is syncing"));
    let prepared = store
        .record_prepared(&claim.id, "txid-1", b"raw", 140)
        .unwrap();
    assert_eq!(prepared.last_error, None);

    let stale = store
        .record_error(&claim.id, "preparing", "", "stale preparation error")
        .unwrap();
    assert_eq!(stale.status, "prepared");
    assert_eq!(stale.last_error, None);
    assert!(store.fail(&claim.id, "stale insufficient funds").is_err());
    let current = store.activity_for_key("guarded").unwrap().unwrap();
    assert_eq!(
        (
            current.status.as_str(),
            current.txid.as_str(),
            current.last_error
        ),
        ("prepared", "txid-1", None)
    );

    store
        .record_error(&claim.id, "prepared", "txid-1", "broadcast unavailable")
        .unwrap();
    let broadcast = store.mark_broadcast(&claim.id, "txid-1").unwrap();
    assert_eq!(broadcast.last_error, None);
    store
        .record_error(&claim.id, "broadcast", "txid-1", "auto-mine failed")
        .unwrap();
    let confirmed = store.confirm(&claim.id, "txid-1", "block").unwrap();
    assert_eq!(confirmed.last_error, None);
    let after = store
        .record_error(&claim.id, "confirmed", "txid-1", "late error")
        .unwrap();
    assert_eq!(after.last_error, None);
    assert!(store.fail(&claim.id, "late failure").is_err());
    assert_eq!(
        store.activity_for_key("guarded").unwrap().unwrap().status,
        "confirmed"
    );

    let doomed = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            99_000,
            "doomed",
            Some(""),
        )
        .unwrap();
    let failed = store.fail(&doomed.id, "insufficient funds").unwrap();
    assert_eq!(
        (failed.status.as_str(), failed.txid.as_str()),
        ("failed", "")
    );
    assert_eq!(failed.last_error.as_deref(), Some("insufficient funds"));
    let again = store.fail(&doomed.id, "a different reason").unwrap();
    assert_eq!(again.last_error.as_deref(), Some("insufficient funds"));
    store
        .record_error(&doomed.id, "failed", "", "overwrite")
        .unwrap();
    assert!(
        store
            .record_prepared(&doomed.id, "late-txid", b"late", 140)
            .is_err()
    );
    assert!(store.mark_broadcast(&doomed.id, "late-txid").is_err());
    store.reset_for_retry(&doomed.id, "").unwrap();
    store.discard_preparing(&doomed.id).unwrap();
    let replay = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            99_000,
            "doomed",
            Some(""),
        )
        .unwrap();
    assert_eq!(replay.id, doomed.id);
    assert_eq!(replay.status, "failed");
    assert_eq!(replay.last_error.as_deref(), Some("insufficient funds"));
    assert!(store.prepared_transaction(&doomed.id).is_err());
    let conflict = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            99_000,
            "doomed",
            None,
        )
        .unwrap_err();
    assert!(conflict.downcast_ref::<IdempotencyConflict>().is_some());
}

#[test]
fn unresolved_intents_include_unprepared_claims_and_exclude_terminal_records() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let absent = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            1_000,
            "absent-memo",
            None,
        )
        .unwrap();
    let empty = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            2_000,
            "empty-memo",
            Some(""),
        )
        .unwrap();
    let text = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "transparent",
            "ironwood",
            3_000,
            "text-memo",
            Some("rent"),
        )
        .unwrap();
    let faucet = store
        .claim_faucet(3, "transparent", 4_000, "faucet")
        .unwrap();
    let broadcast = store
        .claim_transfer(
            2,
            crate::db::Recipient::Account(1),
            "ironwood",
            "transparent",
            5_000,
            "broadcast",
            None,
        )
        .unwrap();
    store
        .record_prepared(&broadcast.id, "broadcast-txid", b"raw", 140)
        .unwrap();
    let broadcast = store
        .mark_broadcast(&broadcast.id, "broadcast-txid")
        .unwrap();
    let confirmed = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(3),
            "ironwood",
            "ironwood",
            6_000,
            "confirmed",
            None,
        )
        .unwrap();
    store
        .record_prepared(&confirmed.id, "confirmed-txid", b"raw", 140)
        .unwrap();
    store
        .confirm(&confirmed.id, "confirmed-txid", "block")
        .unwrap();
    let failed = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(4),
            "ironwood",
            "ironwood",
            7_000,
            "failed",
            None,
        )
        .unwrap();
    store.fail(&failed.id, "insufficient funds").unwrap();

    let intents = store.unresolved_activities().unwrap();
    let summary: Vec<_> = intents
        .iter()
        .map(|intent| {
            (
                intent.activity.id.as_str(),
                intent.key.as_str(),
                intent.memo.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (absent.id.as_str(), "absent-memo", None),
            (empty.id.as_str(), "empty-memo", Some("")),
            (text.id.as_str(), "text-memo", Some("rent")),
            (faucet.id.as_str(), "faucet", None),
            (broadcast.id.as_str(), "broadcast", None),
        ]
    );
    let text = &intents[2].activity;
    assert_eq!(
        (
            text.kind.as_str(),
            text.from_account,
            text.to_account,
            text.source_pool.as_str(),
            text.destination_pool.as_str(),
            text.amount_zatoshi,
            text.txid.as_str(),
            text.status.as_str(),
        ),
        (
            "send",
            Some(1),
            Some(2),
            "transparent",
            "ironwood",
            3_000,
            "",
            "preparing"
        )
    );
    assert_eq!(intents[3].activity.kind, "faucet");
    assert_eq!(intents[3].activity.from_account, None);
    assert_eq!(intents[3].activity.destination_pool, "transparent");
    assert_eq!(intents[4].activity, broadcast);
    assert_eq!(
        store
            .unconfirmed_activities()
            .unwrap()
            .iter()
            .map(|activity| activity.id.as_str())
            .collect::<Vec<_>>(),
        [broadcast.id.as_str()]
    );

    let preparing = store
        .claim_address_faucet("tm-preparing", 10, "address-preparing")
        .unwrap();
    let prepared = store
        .claim_address_faucet("u-prepared", 20, "address-prepared")
        .unwrap();
    let prepared = store
        .record_address_prepared(&prepared.id, "address-txid", b"raw", 140)
        .unwrap();
    let confirmed = store
        .claim_address_faucet("tm-confirmed", 30, "address-confirmed")
        .unwrap();
    store
        .record_address_prepared(&confirmed.id, "confirmed-address-txid", b"raw", 140)
        .unwrap();
    store
        .confirm_address(&confirmed.id, "confirmed-address-txid", "block")
        .unwrap();
    let failed = store
        .claim_address_faucet("tm-failed", 40, "address-failed")
        .unwrap();
    store
        .fail_address(&failed.id, "treasury cannot pay")
        .unwrap();

    assert_eq!(
        store.unresolved_address_faucets().unwrap(),
        [
            AddressFaucetIntent {
                payment: preparing,
                key: "address-preparing".into(),
            },
            AddressFaucetIntent {
                payment: prepared,
                key: "address-prepared".into(),
            },
        ]
    );
}

#[test]
fn repaired_preparation_never_replaces_a_newer_or_terminal_transaction() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "repair",
            None,
        )
        .unwrap();
    store
        .record_prepared(&claim.id, "old-txid", b"old", 140)
        .unwrap();
    store.reset_for_retry(&claim.id, "old-txid").unwrap();
    store
        .record_prepared(&claim.id, "new-txid", b"new", 180)
        .unwrap();

    assert!(
        store
            .record_prepared(&claim.id, "old-txid", b"old", 140)
            .is_err()
    );
    let current = store.activity_for_key("repair").unwrap().unwrap();
    assert_eq!(current.txid, "new-txid");
    assert_eq!(
        store.prepared_transaction(&claim.id).unwrap(),
        PreparedTransaction {
            raw_transaction: b"new".to_vec(),
            expiry_height: 180
        }
    );
    store
        .record_prepared(&claim.id, "new-txid", b"different bytes", 999)
        .unwrap();
    assert_eq!(
        store
            .prepared_transaction(&claim.id)
            .unwrap()
            .raw_transaction,
        b"new"
    );
    store.confirm(&claim.id, "new-txid", "block").unwrap();
    assert!(
        store
            .record_prepared(&claim.id, "other-txid", b"other", 200)
            .is_err()
    );
    let confirmed = store.activity_for_key("repair").unwrap().unwrap();
    assert_eq!(
        (confirmed.status.as_str(), confirmed.txid.as_str()),
        ("confirmed", "new-txid")
    );

    let address = store
        .claim_address_faucet("tm-external", 5, "address-repair")
        .unwrap();
    store
        .record_address_prepared(&address.id, "address-new", b"new", 180)
        .unwrap();
    assert!(
        store
            .record_address_prepared(&address.id, "address-old", b"old", 140)
            .is_err()
    );
    store
        .confirm_address(&address.id, "address-new", "block")
        .unwrap();
    assert!(
        store
            .record_address_prepared(&address.id, "address-other", b"other", 200)
            .is_err()
    );
    let current = store
        .address_faucet_for_key("address-repair")
        .unwrap()
        .unwrap();
    assert_eq!(
        (current.status.as_str(), current.txid.as_str()),
        ("confirmed", "address-new")
    );
    assert_eq!(
        store
            .prepared_transaction(&address.id)
            .unwrap()
            .raw_transaction,
        b"new"
    );
}

#[test]
fn unresolved_external_sends_preserve_recipient_key_and_memo_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    store
        .claim_transfer(
            1,
            Recipient::Address("tmExternal"),
            "ironwood",
            "transparent",
            12000,
            "external-intent",
            Some(""),
        )
        .unwrap();
    let activity = store.activity_for_key("external-intent").unwrap().unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    assert_eq!(
        store.unresolved_activities().unwrap(),
        vec![ActivityIntent {
            activity,
            key: "external-intent".into(),
            memo: Some("".into())
        }]
    );
}

#[test]
fn clearing_payment_errors_requires_the_exact_pending_state_and_transaction() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    for status in ["preparing", "prepared", "broadcast"] {
        let claim = store.claim_faucet(2, "ironwood", 100, status).unwrap();
        let txid = if status == "preparing" {
            ""
        } else {
            "current-txid"
        };
        if status != "preparing" {
            store
                .record_prepared(&claim.id, txid, b"signed bytes", 140)
                .unwrap();
        }
        if status == "broadcast" {
            store.mark_broadcast(&claim.id, txid).unwrap();
        }
        let saved = store
            .record_error(&claim.id, status, txid, "current error")
            .unwrap();
        let stale_status = if status == "preparing" {
            "prepared"
        } else {
            "preparing"
        };
        assert_eq!(
            store.clear_error(&claim.id, stale_status, txid).unwrap(),
            saved
        );
        assert_eq!(
            store.clear_error(&claim.id, status, "stale-txid").unwrap(),
            saved
        );
        let cleared = store.clear_error(&claim.id, status, txid).unwrap();
        assert_eq!(cleared.last_error, None);
        assert_eq!(
            (cleared.status.as_str(), cleared.txid.as_str()),
            (status, txid)
        );
        assert_eq!(store.clear_error(&claim.id, status, txid).unwrap(), cleared);
    }
    let failed = store
        .claim_faucet(2, "ironwood", 100, "failed-clear")
        .unwrap();
    let failed = store.fail(&failed.id, "terminal failure").unwrap();
    assert_eq!(store.clear_error(&failed.id, "failed", "").unwrap(), failed);
    assert_eq!(
        store.clear_error(&failed.id, "preparing", "").unwrap(),
        failed
    );

    let confirmed = store
        .claim_faucet(2, "ironwood", 100, "confirmed-clear")
        .unwrap();
    store
        .record_prepared(&confirmed.id, "included-txid", b"signed bytes", 140)
        .unwrap();
    store
        .confirm(&confirmed.id, "included-txid", "included-block")
        .unwrap();
    // A retained terminal diagnostic must survive an exact-status clear too.
    store
        .0
        .lock()
        .unwrap()
        .execute(
            "UPDATE activity SET last_error='retained terminal diagnostic' WHERE id=?1",
            [&confirmed.id],
        )
        .unwrap();
    let confirmed = store.activity_for_key("confirmed-clear").unwrap().unwrap();
    assert_eq!(
        store
            .clear_error(&confirmed.id, "confirmed", "included-txid")
            .unwrap(),
        confirmed
    );
    assert_eq!(
        store
            .clear_error(&confirmed.id, "broadcast", "included-txid")
            .unwrap(),
        confirmed
    );
}
