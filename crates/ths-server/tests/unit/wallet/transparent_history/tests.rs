use super::*;
use crate::db::{Store, TREASURY_ACCOUNT_ID};
use zcash_client_backend::data_api::{
    chain::ChainState,
    scanning::{ScanPriority, ScanRange},
};

use crate::wallet::transparent_history_test_support as support;

async fn import(wallet: &RealWallet, raw: RawTransaction) -> TxId {
    let tx = HistoryTransaction::decode(raw, 2, 5).await.unwrap();
    let id = tx.transaction.txid();
    wallet.import_transparent_history(vec![tx]).await.unwrap();
    id
}

#[tokio::test]
async fn external_transparent_only_spend_updates_sdk_balances_without_deleting_receipts() {
    let (dir, store, wallet) = support::setup(5).await;
    let receiver = store.account(2).unwrap().transparent_address;
    let funding = import(
        &wallet,
        support::transaction(&receiver, OutPoint::new([9; 32], 0), 1, 2),
    )
    .await;
    let control = import(
        &wallet,
        support::transaction(&receiver, OutPoint::new([8; 32], 0), 2, 2),
    )
    .await;
    let mut accounts = store.accounts().unwrap();
    wallet.apply_balances(&mut accounts).await.unwrap();
    assert_eq!(accounts[1].transparent_zatoshi, 20_000_000);
    let spender = support::transaction(
        &store.account(3).unwrap().transparent_address,
        OutPoint::new(*funding.as_ref(), 0),
        3,
        3,
    );
    let spend_id = import(&wallet, spender.clone()).await;
    import(&wallet, spender).await;
    wallet.apply_balances(&mut accounts).await.unwrap();
    assert_eq!(
        accounts[1].transparent_zatoshi, 10_000_000,
        "unspent control remains counted"
    );
    assert_eq!(accounts[2].transparent_zatoshi, 10_000_000);
    let connection = rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap();
    let receipts: u32 = connection
        .query_row(
            "SELECT COUNT(*) FROM transparent_received_outputs",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(receipts, 3, "replay must not duplicate or delete receipts");
    assert_eq!(
        wallet
            .known_spending_transactions(&funding.to_string(), 0)
            .await
            .unwrap(),
        vec![spend_id.to_string()]
    );
    assert!(
        wallet
            .known_spending_transactions(&control.to_string(), 0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn history_coverage_is_public_incremental_and_survives_reopen() {
    let (dir, store, wallet) = support::setup(5).await;
    let queries = wallet.transparent_history_queries().await.unwrap();
    assert!(!queries.is_empty());
    assert!(queries.iter().all(|q| q.cursor.is_none()));
    let treasury = store
        .account(TREASURY_ACCOUNT_ID)
        .unwrap()
        .transparent_address;
    assert!(queries.iter().all(|q| q.address != treasury));
    let query = queries
        .into_iter()
        .find(|q| q.address == store.account(2).unwrap().transparent_address)
        .unwrap();
    let checkpoint = ChainCheckpoint {
        height: 5,
        hash: BlockHash([5; 32]).to_string(),
    };
    wallet
        .advance_transparent_history(query.clone(), checkpoint.clone())
        .await
        .unwrap();
    assert!(
        wallet
            .advance_transparent_history(query, checkpoint.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("cursor changed")
    );
    drop(wallet);
    let reopened = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    let queries = reopened.transparent_history_queries().await.unwrap();
    assert_eq!(
        queries
            .iter()
            .find(|q| q.address == store.account(2).unwrap().transparent_address)
            .unwrap()
            .cursor,
        Some(checkpoint)
    );
    assert!(
        queries
            .iter()
            .filter(|q| q.address != store.account(2).unwrap().transparent_address)
            .all(|q| q.cursor.is_none())
    );
}

#[tokio::test]
async fn legacy_wallet_gets_empty_history_coverage_without_changing_old_migrations() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let mut db = WalletDb::for_path(
        dir.path().join("wallet.db"),
        regtest_network(),
        SystemClock,
        UnwrapErr(SysRng),
    )
    .unwrap();
    WalletMigrator::new()
        .with_seed(SecretVec::new(hex::decode(store.seed().unwrap()).unwrap()))
        .with_external_migrations(vec![
            Box::new(PreparedPaymentsMigration),
            Box::new(recovery::TreasuryCursorMigration),
        ])
        .init_or_migrate(&mut db)
        .unwrap();
    drop(db);
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    assert!(
        wallet
            .transparent_history_queries()
            .await
            .unwrap()
            .iter()
            .all(|q| q.cursor.is_none())
    );
    assert!(wallet.treasury_cursor().await.unwrap().is_none());
}

#[tokio::test]
async fn import_is_atomic_and_cannot_run_ahead_of_compact_scanning() {
    let (dir, store, wallet) = support::setup(5).await;
    let receiver = store.account(2).unwrap().transparent_address;
    let first = HistoryTransaction::decode(
        support::transaction(&receiver, OutPoint::new([8; 32], 0), 1, 2),
        2,
        6,
    )
    .await
    .unwrap();
    let later = HistoryTransaction::decode(
        support::transaction(&receiver, OutPoint::new([9; 32], 0), 2, 6),
        2,
        6,
    )
    .await
    .unwrap();
    wallet
        .import_transparent_history(vec![first, later])
        .await
        .unwrap_err();
    let count: u32 = rusqlite::Connection::open(dir.path().join("wallet.db"))
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM transparent_received_outputs",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 0,
        "an invalid later item must roll back the whole chunk"
    );
}

#[tokio::test]
async fn rewind_couples_history_and_sdk_state_and_allows_a_replacement_spend() {
    let (dir, store, wallet) = support::setup(5).await;
    let receiver = store.account(2).unwrap().transparent_address;
    let funding = import(
        &wallet,
        support::transaction(&receiver, OutPoint::new([9; 32], 0), 1, 2),
    )
    .await;
    let spend = support::transaction(
        &store.account(3).unwrap().transparent_address,
        OutPoint::new(*funding.as_ref(), 0),
        2,
        3,
    );
    let old_spend = import(&wallet, spend).await;
    let query = wallet
        .transparent_history_queries()
        .await
        .unwrap()
        .into_iter()
        .find(|q| q.address == receiver)
        .unwrap();
    wallet
        .advance_transparent_history(
            query,
            ChainCheckpoint {
                height: 5,
                hash: BlockHash([5; 32]).to_string(),
            },
        )
        .await
        .unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_history_rewind BEFORE UPDATE ON ext_ths_transparent_history BEGIN SELECT RAISE(ABORT,'injected history rewind failure'); END").unwrap();
    assert!(
        wallet
            .rewind_to_height(ChainState::empty(2.into(), BlockHash([2; 32])))
            .await
            .is_err()
    );
    assert_eq!(
        wallet.scanned_checkpoint().await.unwrap().unwrap().height,
        5
    );
    assert_eq!(
        wallet.transparent_history_checkpoints().await.unwrap()[0].height,
        5
    );
    conn.execute_batch("DROP TRIGGER fail_history_rewind")
        .unwrap();
    wallet
        .rewind_to_height(ChainState::empty(2.into(), BlockHash([2; 32])))
        .await
        .unwrap();
    assert_eq!(
        wallet.transparent_history_checkpoints().await.unwrap(),
        vec![ChainCheckpoint {
            height: 2,
            hash: BlockHash([2; 32]).to_string()
        }]
    );
    let mut accounts = store.accounts().unwrap();
    wallet.apply_balances(&mut accounts).await.unwrap();
    assert_eq!(
        accounts[1].transparent_zatoshi, 10_000_000,
        "orphaned spend must stop reducing canonical holdings"
    );
    wallet.update_chain_tip(3).await.unwrap();
    wallet
        .scan_batch(
            ScanRange::from_parts(3.into()..4.into(), ScanPriority::Historic),
            vec![CompactBlock {
                height: 3,
                hash: vec![33; 32],
                prev_hash: vec![2; 32],
                chain_metadata: Some(Default::default()),
                ..Default::default()
            }],
            ChainState::empty(2.into(), BlockHash([2; 32])),
        )
        .await
        .unwrap();
    let replacement = support::transaction(
        &store.account(4).unwrap().transparent_address,
        OutPoint::new(*funding.as_ref(), 0),
        3,
        3,
    );
    let replacement_id = import(&wallet, replacement).await;
    wallet.apply_balances(&mut accounts).await.unwrap();
    assert_eq!(accounts[1].transparent_zatoshi, 0);
    assert_eq!(accounts[3].transparent_zatoshi, 10_000_000);
    let spenders = wallet
        .known_spending_transactions(&funding.to_string(), 0)
        .await
        .unwrap();
    assert!(
        spenders.contains(&old_spend.to_string()) && spenders.contains(&replacement_id.to_string())
    );
}

#[tokio::test]
async fn history_decoder_rejects_unmined_out_of_range_malformed_and_trailing_data() {
    let (_dir, store, _wallet) = support::setup(5).await;
    let raw = support::transaction(
        &store.account(2).unwrap().transparent_address,
        OutPoint::new([9; 32], 0),
        1,
        2,
    );
    for height in [0, 1, 6, u64::MAX] {
        let mut invalid = raw.clone();
        invalid.height = height;
        assert!(HistoryTransaction::decode(invalid, 2, 5).await.is_err());
    }
    let mut trailing = raw;
    trailing.data.push(0);
    assert!(HistoryTransaction::decode(trailing, 2, 5).await.is_err());
    assert!(
        HistoryTransaction::decode(
            RawTransaction {
                data: vec![0],
                height: 2
            },
            2,
            5
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn pending_spend_does_not_hide_a_canonical_replacement_or_change_a_payment_journal() {
    let (dir, store, wallet) = support::setup(5).await;
    let receiver = store.account(2).unwrap().transparent_address;
    let funding = import(
        &wallet,
        support::transaction(&receiver, OutPoint::new([9; 32], 0), 1, 2),
    )
    .await;
    let raw_pending = support::transaction(
        &store.account(3).unwrap().transparent_address,
        OutPoint::new(*funding.as_ref(), 0),
        2,
        3,
    );
    let pending = HistoryTransaction::decode(raw_pending, 2, 5).await.unwrap();
    wallet.with_db(move |db| {
        decrypt_and_store_transaction(&regtest_network(),db,&pending.transaction,None)?;
        db.transactionally_with_extension::<_,_,anyhow::Error>(|_,ext| {
            ext.execute("INSERT INTO ext_tsz_prepared_payments VALUES('retained-payment','original-txid',x'010203',200)",[])?;
            Ok(())
        })
    }).await.unwrap();
    let mut accounts = store.accounts().unwrap();
    wallet.apply_balances(&mut accounts).await.unwrap();
    assert_eq!(
        accounts[1].transparent_zatoshi, 10_000_000,
        "a pending reservation is not a canonical debit"
    );
    let replacement = support::transaction(
        &store.account(4).unwrap().transparent_address,
        OutPoint::new(*funding.as_ref(), 0),
        3,
        4,
    );
    import(&wallet, replacement).await;
    wallet.apply_balances(&mut accounts).await.unwrap();
    assert_eq!(accounts[1].transparent_zatoshi, 0);
    assert_eq!(accounts[3].transparent_zatoshi, 10_000_000);
    let journal = wallet.prepared("retained-payment").await.unwrap().unwrap();
    assert_eq!(journal.raw_transaction, vec![1, 2, 3]);
    assert_eq!(journal.txid, "original-txid");
    let tx_count: u32 = rusqlite::Connection::open(dir.path().join("wallet.db"))
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM transactions WHERE raw IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        tx_count, 3,
        "both pending and canonical transaction records remain stored"
    );
}

#[tokio::test]
async fn failed_history_cursor_commit_stays_retryable_and_rejects_wrong_checkpoints() {
    let (dir, store, wallet) = support::setup(5).await;
    let query = wallet
        .transparent_history_queries()
        .await
        .unwrap()
        .into_iter()
        .find(|q| q.address == store.account(2).unwrap().transparent_address)
        .unwrap();
    for checkpoint in [
        ChainCheckpoint {
            height: 6,
            hash: BlockHash([6; 32]).to_string(),
        },
        ChainCheckpoint {
            height: 5,
            hash: "ff".repeat(32),
        },
    ] {
        assert!(
            wallet
                .advance_transparent_history(query.clone(), checkpoint)
                .await
                .is_err()
        );
    }
    let conn = rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_history_cursor BEFORE INSERT ON ext_ths_transparent_history BEGIN SELECT RAISE(ABORT,'injected cursor commit failure'); END").unwrap();
    let checkpoint = ChainCheckpoint {
        height: 5,
        hash: BlockHash([5; 32]).to_string(),
    };
    assert!(
        wallet
            .advance_transparent_history(query.clone(), checkpoint.clone())
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
    conn.execute_batch("DROP TRIGGER fail_history_cursor")
        .unwrap();
    wallet
        .advance_transparent_history(query, checkpoint.clone())
        .await
        .unwrap();
    assert_eq!(
        wallet.transparent_history_checkpoints().await.unwrap(),
        vec![checkpoint]
    );
}

#[tokio::test]
async fn newly_generated_receiver_does_not_inherit_another_receivers_history_coverage() {
    let (_dir, _store, wallet) = support::setup(5).await;
    let checkpoint = ChainCheckpoint {
        height: 5,
        hash: BlockHash([5; 32]).to_string(),
    };
    let before = wallet.transparent_history_queries().await.unwrap();
    for query in before.clone() {
        wallet
            .advance_transparent_history(query, checkpoint.clone())
            .await
            .unwrap();
    }
    let account_id = wallet.account_ids[0];
    let address = wallet
        .with_db(move |db| {
            let ua = db
                .get_address_for_index(
                    account_id,
                    zip32::DiversifierIndex::from(1000_u32),
                    zcash_keys::keys::UnifiedAddressRequest::AllAvailableKeys,
                )?
                .context("new receiver missing")?;
            Ok(ua
                .transparent()
                .context("transparent receiver missing")?
                .encode(&regtest_network()))
        })
        .await
        .unwrap();
    assert!(before.iter().all(|query| query.address != address));
    let after = wallet.transparent_history_queries().await.unwrap();
    assert!(
        after
            .iter()
            .find(|query| query.address == address)
            .unwrap()
            .cursor
            .is_none()
    );
    assert!(
        before.iter().all(|previous| after
            .iter()
            .any(|query| query.address == previous.address
                && query.cursor == Some(checkpoint.clone())))
    );
}
