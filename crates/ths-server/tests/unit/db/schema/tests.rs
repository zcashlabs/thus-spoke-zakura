use super::*;

#[test]
fn existing_activity_rows_gain_a_stable_first_seen_order() {
    let store = Store::open(":memory:").unwrap();
    store
        .0
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TABLE activity (
                id TEXT PRIMARY KEY, kind TEXT NOT NULL, from_account INTEGER,
                to_account INTEGER, to_address TEXT, source_pool TEXT NOT NULL,
                destination_pool TEXT NOT NULL, amount_zatoshi INTEGER NOT NULL,
                txid TEXT NOT NULL, block_hash TEXT, status TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, last_error TEXT
            );
            INSERT INTO activity(id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,status)
                VALUES('first','send',1,2,'ironwood','ironwood',1,'','preparing'),
                      ('second','send',1,2,'ironwood','ironwood',1,'','preparing');",
        )
        .unwrap();

    store.initialize().unwrap();
    let initial = store
        .activity_sequences(&["second".to_owned(), "first".to_owned()])
        .unwrap();
    assert!(initial["first"] < initial["second"]);

    store.initialize().unwrap();
    let reopened = store
        .activity_sequences(&["first".to_owned(), "second".to_owned()])
        .unwrap();
    assert_eq!(reopened, initial);
}

#[test]
fn restores_the_hidden_treasury_for_existing_stores() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    store
        .0
        .lock()
        .unwrap()
        .execute("DELETE FROM accounts WHERE id=?1", [TREASURY_ACCOUNT_ID])
        .unwrap();
    store.initialize().unwrap();
    assert_eq!(store.accounts().unwrap().len(), 6);
    assert_eq!(store.user_accounts().unwrap().len(), 5);
}
#[test]
fn existing_idempotency_tables_gain_a_memo_column() {
    let store = Store::open(":memory:").unwrap();
    store
        .0
        .lock()
        .unwrap()
        .execute_batch("CREATE TABLE idempotency (key TEXT PRIMARY KEY, activity_id TEXT NOT NULL)")
        .unwrap();
    store.initialize().unwrap();
    store
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
            .0
            .lock()
            .unwrap()
            .query_row(
                "SELECT memo FROM idempotency WHERE key='memo-key'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "rent"
    );
}

#[test]
fn recipient_and_status_upgrades_preserve_errors_and_signed_payments_in_both_orders() {
    for recipients_first in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.db");
        let db = Connection::open(&path).unwrap();
        let recipients = if recipients_first {
            "to_account INTEGER, to_address TEXT,"
        } else {
            "to_account INTEGER NOT NULL,"
        };
        let error = if recipients_first {
            ""
        } else {
            ", last_error TEXT"
        };
        db.execute_batch(&format!("CREATE TABLE activity (
            id TEXT PRIMARY KEY, kind TEXT NOT NULL, from_account INTEGER,
            {recipients} source_pool TEXT NOT NULL, destination_pool TEXT NOT NULL,
            amount_zatoshi INTEGER NOT NULL, txid TEXT NOT NULL, block_hash TEXT,
            status TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP {error});
            CREATE TABLE idempotency(key TEXT PRIMARY KEY, activity_id TEXT NOT NULL, memo TEXT);
            CREATE TABLE prepared_payments(activity_id TEXT PRIMARY KEY, raw_transaction BLOB NOT NULL, expiry_height INTEGER NOT NULL);
            INSERT INTO activity(id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,status)
                VALUES('failed','send',1,2,'ironwood','ironwood',12000,'','failed'),
                      ('signed','send',1,3,'ironwood','ironwood',13000,'original-txid','broadcast');
            INSERT INTO idempotency VALUES('failed-key','failed',''),('signed-key','signed','memo');
            INSERT INTO prepared_payments VALUES('signed',x'010203',140);" )).unwrap();
        db.execute("UPDATE activity SET created_at='2025-01-02 03:04:05'", [])
            .unwrap();
        if recipients_first {
            db.execute_batch("INSERT INTO activity(id,kind,from_account,to_account,to_address,source_pool,destination_pool,amount_zatoshi,txid,status,created_at)
                VALUES('external','send',2,NULL,'tmExternal','ironwood','transparent',14000,'','preparing','2025-02-03 04:05:06');
                INSERT INTO idempotency VALUES('external-key','external','');").unwrap();
        }
        if !recipients_first {
            db.execute(
                "UPDATE activity SET last_error='original failure' WHERE id='failed'",
                [],
            )
            .unwrap();
        }
        drop(db);
        for _ in 0..2 {
            let store = Store::open(&path).unwrap();
            store.initialize().unwrap();
            let failed = store.activity_for_key("failed-key").unwrap().unwrap();
            assert_eq!(failed.id, "failed");
            assert_eq!(failed.status, "failed");
            assert_eq!(failed.created_at, "2025-01-02 03:04:05");
            assert_eq!(failed.to_account, Some(2));
            assert_eq!(failed.to_address, None);
            assert_eq!(
                failed.last_error.as_deref(),
                if recipients_first {
                    None
                } else {
                    Some("original failure")
                }
            );
            let order: Vec<_> = store
                .activities(100)
                .unwrap()
                .into_iter()
                .map(|activity| activity.id)
                .collect();
            assert_eq!(
                order,
                if recipients_first {
                    vec!["external", "signed", "failed"]
                } else {
                    vec!["signed", "failed"]
                }
            );
            let intents = store.unresolved_activities().unwrap();
            assert_eq!(intents.len(), if recipients_first { 2 } else { 1 });
            assert_eq!(intents[0].activity.id, "signed");
            assert_eq!(intents[0].activity.status, "broadcast");
            assert_eq!(intents[0].activity.created_at, "2025-01-02 03:04:05");
            assert_eq!(intents[0].key, "signed-key");
            if recipients_first {
                let external = &intents[1];
                assert_eq!(external.activity.id, "external");
                assert_eq!(external.activity.to_account, None);
                assert_eq!(external.activity.to_address.as_deref(), Some("tmExternal"));
                assert_eq!(external.activity.created_at, "2025-02-03 04:05:06");
                assert_eq!(external.key, "external-key");
                assert_eq!(external.memo.as_deref(), Some(""));
                assert_eq!(external.activity.status, "preparing");
                assert_eq!(external.activity.txid, "");
                assert_eq!(external.activity.last_error, None);
            }
            assert_eq!(intents[0].memo.as_deref(), Some("memo"));
            assert_eq!(intents[0].activity.txid, "original-txid");
            assert_eq!(
                store.prepared_transaction("signed").unwrap(),
                PreparedTransaction {
                    raw_transaction: vec![1, 2, 3],
                    expiry_height: 140
                }
            );
        }
    }
}

#[test]
fn older_stores_gain_payment_errors_without_losing_claims() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    {
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE activity (
                 id TEXT PRIMARY KEY, kind TEXT NOT NULL, from_account INTEGER,
                 to_account INTEGER NOT NULL, source_pool TEXT NOT NULL,
                 destination_pool TEXT NOT NULL, amount_zatoshi INTEGER NOT NULL,
                 txid TEXT NOT NULL, block_hash TEXT, status TEXT NOT NULL,
                 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             CREATE TABLE idempotency (key TEXT PRIMARY KEY, activity_id TEXT NOT NULL);
             CREATE TABLE prepared_payments (
                 activity_id TEXT PRIMARY KEY, raw_transaction BLOB NOT NULL,
                 expiry_height INTEGER NOT NULL
             );
             CREATE TABLE address_faucets (
                 key TEXT PRIMARY KEY, id TEXT UNIQUE NOT NULL, address TEXT NOT NULL,
                 amount_zatoshi INTEGER NOT NULL, txid TEXT NOT NULL DEFAULT '',
                 block_hash TEXT, status TEXT NOT NULL DEFAULT 'preparing'
             );
             INSERT INTO activity(id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,status)
                 VALUES('legacy-send','send',1,2,'ironwood','ironwood',12000,'legacy-txid','broadcast');
             INSERT INTO idempotency(key,activity_id) VALUES('legacy-key','legacy-send');
             INSERT INTO prepared_payments VALUES('legacy-send',x'0102',140);
             INSERT INTO address_faucets(key,id,address,amount_zatoshi) VALUES('legacy-address','legacy-faucet','tm-external',7);",
        )
        .unwrap();
    }

    for _ in 0..2 {
        let store = Store::open(&path).unwrap();
        store.initialize().unwrap();
        let send = store
            .claim_transfer(
                1,
                crate::db::Recipient::Account(2),
                "ironwood",
                "ironwood",
                12_000,
                "legacy-key",
                None,
            )
            .unwrap();
        assert_eq!(
            (send.id.as_str(), send.txid.as_str(), send.status.as_str()),
            ("legacy-send", "legacy-txid", "broadcast")
        );
        assert_eq!(send.last_error, None);
        assert_eq!(
            store.prepared_transaction(&send.id).unwrap(),
            crate::db::PreparedTransaction {
                raw_transaction: vec![1, 2],
                expiry_height: 140
            }
        );
        let faucet = store
            .claim_address_faucet("tm-external", 7, "legacy-address")
            .unwrap();
        assert_eq!(faucet.id, "legacy-faucet");
        assert_eq!(faucet.last_error, None);
    }

    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    store
        .record_error(
            "legacy-send",
            "broadcast",
            "legacy-txid",
            "lookup unavailable",
        )
        .unwrap();
    store
        .fail_address("legacy-faucet", "treasury cannot pay")
        .unwrap();
    drop(store);
    let reopened = Store::open(&path).unwrap();
    reopened.initialize().unwrap();
    assert_eq!(
        reopened
            .activity_for_key("legacy-key")
            .unwrap()
            .unwrap()
            .last_error
            .as_deref(),
        Some("lookup unavailable")
    );
    let failed = reopened
        .address_faucet_for_key("legacy-address")
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, "failed");
    assert_eq!(failed.last_error.as_deref(), Some("treasury cannot pay"));
}

#[test]
fn migrates_activity_created_before_address_sends() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    {
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE activity (
                 id TEXT PRIMARY KEY, kind TEXT NOT NULL, from_account INTEGER,
                 to_account INTEGER NOT NULL, source_pool TEXT NOT NULL,
                 destination_pool TEXT NOT NULL, amount_zatoshi INTEGER NOT NULL,
                 txid TEXT NOT NULL, block_hash TEXT, status TEXT NOT NULL,
                 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             INSERT INTO activity(id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,status)
             VALUES('older','send',1,2,'ironwood','ironwood',1,'txid-older','confirmed'),
                   ('newer','send',1,3,'ironwood','ironwood',2,'txid-newer','broadcast');",
        )
        .unwrap();
    }

    let store = Store::open(&path).unwrap();
    store.initialize().unwrap();
    let activity = store.activities(10).unwrap();
    assert_eq!(
        activity.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
        ["newer", "older"]
    );
    assert_eq!(activity[0].to_account, Some(3));
    assert_eq!(activity[0].to_address, None);

    store
        .claim_transfer(
            1,
            Recipient::Address("tmExternal"),
            "ironwood",
            "transparent",
            3,
            "after-migration",
            None,
        )
        .unwrap();
    // Initializing again must keep rows written after the migration.
    store.initialize().unwrap();
    let claimed = store.activity_for_key("after-migration").unwrap().unwrap();
    assert_eq!(claimed.to_address.as_deref(), Some("tmExternal"));
    assert_eq!(store.activities(10).unwrap().len(), 3);
}
