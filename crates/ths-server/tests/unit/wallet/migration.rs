use super::*;
use crate::db::{Recipient, Store, USER_ACCOUNT_COUNT};
use serde_json::Value;

fn fixture() -> (tempfile::TempDir, Value) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("wallet.db"),
        include_bytes!("../../fixtures/wallet-rc7/wallet.sqlite"),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("ths.db"),
        include_bytes!("../../fixtures/wallet-rc7/ths.sqlite"),
    )
    .unwrap();
    let expected =
        serde_json::from_str(include_str!("../../fixtures/wallet-rc7/expected.json")).unwrap();
    (dir, expected)
}

async fn assert_retained_contract(path: &Path, expected: &Value) {
    let store = Store::open(path.join("ths.db")).unwrap();
    store.initialize().unwrap();
    assert_eq!(store.seed().unwrap(), expected["seed_hex"]);
    assert_eq!(
        store.user_accounts().unwrap().len(),
        USER_ACCOUNT_COUNT as usize
    );
    for account in store.accounts().unwrap() {
        let original = &expected["accounts"][usize::from(account.id - 1)];
        assert_eq!(account.name, original[1]);
        assert_eq!(account.unified_address, original[2]);
        assert_eq!(account.transparent_address, original[3]);
    }
    let wallet = RealWallet::open(path, &store.seed().unwrap()).unwrap();
    for (index, id) in wallet.account_ids.iter().enumerate() {
        assert_eq!(
            id.expose_uuid().to_string(),
            expected["wallet_accounts"][index][1]
        );
    }
    wallet.public_transaction_enhancements(100).await.unwrap();
    let mut accounts = store.accounts().unwrap();
    wallet.apply_balances(&mut accounts).await.unwrap();
    assert_eq!(accounts[0].transparent_zatoshi, 20_000_000);
    assert_eq!(accounts[0].ironwood_zatoshi, 100_000_000);
    assert!(
        accounts[1..]
            .iter()
            .all(|account| account.transparent_zatoshi == 0 && account.ironwood_zatoshi == 0)
    );

    let original = &expected["prepared"];
    let id = original["activity_id"].as_str().unwrap();
    let before = store
        .activity_for_key("rc7-upgrade-payment")
        .unwrap()
        .unwrap();
    assert_eq!(before.id, id);
    assert_eq!(before.status, "prepared");
    assert_eq!(before.txid, original["txid"]);
    let replay = store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "transparent",
            1_000_000,
            "rc7-upgrade-payment",
            None,
        )
        .unwrap();
    assert_eq!(replay, before);
    assert!(
        store
            .claim_transfer(
                1,
                Recipient::Account(2),
                "ironwood",
                "transparent",
                2_000_000,
                "rc7-upgrade-payment",
                None
            )
            .is_err()
    );
    let prepared = wallet
        .prepare(
            Some(id),
            None,
            "invalid seed",
            0,
            "invalid pool",
            "invalid address",
            0,
            None,
            Ok,
        )
        .await
        .unwrap();
    assert_eq!(prepared.txid, original["txid"]);
    assert_eq!(hex::encode(&prepared.raw_transaction), original["raw_hex"]);
    assert_eq!(
        prepared.expiry_height,
        original["expiry_height"].as_u64().unwrap()
    );
    let server_prepared = store.prepared_transaction(id).unwrap();
    assert_eq!(server_prepared.raw_transaction, prepared.raw_transaction);
    assert_eq!(server_prepared.expiry_height, prepared.expiry_height);
    let transaction = Transaction::read(&prepared.raw_transaction[..], BranchId::Nu7).unwrap();
    assert_eq!(transaction.txid().to_string(), prepared.txid);
    assert_eq!(
        u64::from(u32::from(transaction.expiry_height())),
        prepared.expiry_height
    );

    let cursor = wallet.treasury_cursor().await.unwrap().unwrap();
    assert_eq!(
        u64::from(cursor.height),
        expected["height"].as_u64().unwrap()
    );
    assert_eq!(cursor.receiver, accounts[5].transparent_address);
    assert_eq!(cursor.block_hash, expected["treasury_cursor"][2]);
    let checkpoints = wallet.transparent_history_checkpoints().await.unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(
        checkpoints[0].height,
        expected["history_cursor"][1].as_u64().unwrap()
    );
    assert_eq!(checkpoints[0].hash, expected["history_cursor"][2]);
    let (history, journals) = wallet
        .canonical_activity(100, vec![id.to_owned()])
        .await
        .unwrap();
    assert!(!history.is_empty());
    assert!(history.iter().any(|entry| {
        entry
            .destination_pools
            .iter()
            .any(|pool| pool == "ironwood")
    }));
    assert_eq!(journals[id], prepared.txid);
}

#[tokio::test]
async fn rc7_wallet_migration_preserves_accounts_balances_history_and_prepared_payment() {
    let (dir, expected) = fixture();
    assert_retained_contract(dir.path(), &expected).await;
    assert_retained_contract(dir.path(), &expected).await;
}

#[tokio::test]
async fn interrupted_rc7_migration_resumes_without_resetting_retained_state() {
    let (dir, expected) = fixture();
    let db = rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap();
    let initial: u64 = db
        .query_row("SELECT COUNT(*) FROM schemer_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    db.execute_batch(&format!(
        "CREATE TRIGGER interrupt_upgrade BEFORE INSERT ON schemer_migrations
         WHEN (SELECT COUNT(*) FROM schemer_migrations) > {initial}
         BEGIN SELECT RAISE(ABORT, 'injected upgrade interruption'); END;"
    ))
    .unwrap();
    let error = RealWallet::open(dir.path(), expected["seed_hex"].as_str().unwrap())
        .err()
        .expect("migration must report the injected failure");
    assert!(
        format!("{error:#}").contains("injected upgrade interruption"),
        "{error:#}"
    );
    let committed: u64 = db
        .query_row("SELECT COUNT(*) FROM schemer_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(
        committed > initial,
        "at least one migration must have committed before interruption"
    );
    db.execute_batch("DROP TRIGGER interrupt_upgrade").unwrap();
    drop(db);
    assert_retained_contract(dir.path(), &expected).await;
    assert_retained_contract(dir.path(), &expected).await;
}
