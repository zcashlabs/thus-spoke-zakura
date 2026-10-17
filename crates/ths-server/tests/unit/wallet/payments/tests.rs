use crate::wallet::tests::test_wallet;
use transparent::{
    bundle::{OutPoint, TxOut},
    keys::TransparentKeyScope,
};
use zcash_client_backend::{
    data_api::testing::{
        self, AddressType, IronwoodFvk, TestBuilder, orchard::OrchardPoolTester,
        pool::ShieldedPoolTester,
    },
    wallet::WalletTransparentOutput,
};
use zcash_client_sqlite::testing::{
    BlockCache,
    db::{TestDb, TestDbFactory},
};
use zcash_keys::keys::UnifiedAddressRequest;
use zcash_primitives::block::BlockHash;

use super::*;

type TestState = testing::TestState<BlockCache, TestDb, LocalNetwork>;

/// The SDK's default test network with every upgrade through NU7 active from
/// Sapling activation, like the local chain, so shielded value lives in Ironwood.
fn test_state() -> TestState {
    let activation = Some(BlockHeight::from_u32(100_000));
    let network = LocalNetwork {
        nu6: activation,
        nu6_1: activation,
        nu6_2: activation,
        nu6_3: activation,
        nu7: activation,
        ..TestBuilder::<(), ()>::DEFAULT_NETWORK
    };
    TestBuilder::new()
        .with_network(network)
        .with_data_store_factory(TestDbFactory::default())
        .with_block_cache(BlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build()
}

fn add_empty_blocks(st: &mut TestState, count: usize) {
    for _ in 0..count {
        let (height, _) = st.generate_empty_block();
        st.scan_cached_blocks(height, 1);
    }
}

/// Receives one Ironwood (version 3) note of `value`, then advances the chain so
/// the note's shard is scanned and an anchor is available to spend it.
fn fund_ironwood(st: &mut TestState, value: u64) {
    let account_id = st.test_account().unwrap().id();
    let fvk = IronwoodFvk(OrchardPoolTester::test_account_fvk(st));
    let value = Zatoshis::const_from_u64(value);
    let (height, _, _) = st.generate_next_block(&fvk, AddressType::DefaultExternal, value);
    st.scan_cached_blocks(height, 1);
    assert_eq!(st.get_total_balance(account_id), value);
    add_empty_blocks(st, 5);
}

fn recipient(st: &TestState, pool: &str) -> Address {
    let account_id = st.test_account().unwrap().id();
    let ua = st
        .wallet()
        .get_last_generated_address_matching(account_id, UnifiedAddressRequest::AllAvailableKeys)
        .unwrap()
        .unwrap();
    if pool == "transparent" {
        Address::Transparent(*ua.transparent().unwrap())
    } else {
        Address::Unified(ua)
    }
}

fn quote(st: &mut TestState, source_pool: &str, destination_pool: &str) -> Result<SendQuote> {
    let recipient = recipient(st, destination_pool);
    let account_id = st.test_account().unwrap().id();
    let params = *st.network();
    RealWallet::quote(
        st.wallet_mut(),
        &params,
        account_id,
        source_pool,
        &recipient,
    )
}

fn fund_transparent(st: &mut TestState, value: u64) {
    add_empty_blocks(st, 10);
    let account_id = st.test_account().unwrap().id();
    let taddr = match recipient(st, "transparent") {
        Address::Transparent(address) => address,
        _ => unreachable!(),
    };
    let utxo = WalletTransparentOutput::from_parts(
        OutPoint::new([1; 32], 0),
        TxOut::new(Zatoshis::const_from_u64(value), taddr.script().into()),
        Some(st.wallet().chain_height().unwrap().unwrap()),
        Some(account_id),
        Some(TransparentKeyScope::EXTERNAL),
        None,
    )
    .unwrap();
    st.wallet_mut()
        .put_received_transparent_utxo(&utxo)
        .unwrap();
}

#[tokio::test]
async fn canonical_balances_ignore_unmined_effects_without_deleting_them() {
    let mut st = test_state();
    fund_ironwood(&mut st, 100_000_000);
    fund_transparent(&mut st, 20_000_000);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wallet.db");
    st.wallet()
        .conn()
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    let wallet = RealWallet {
        db: Arc::new(Mutex::new(
            WalletDb::for_path(&path, *st.network(), SystemClock, UnwrapErr(SysRng)).unwrap(),
        )),
        account_ids: vec![st.test_account().unwrap().id()],
        lightwalletd: String::new(),
    };
    let store = crate::db::Store::open(dir.path().join("ths.db")).unwrap();
    store.initialize().unwrap();
    let mut accounts = vec![store.account(1).unwrap()];
    wallet.apply_balances(&mut accounts).await.unwrap();
    assert_eq!(accounts[0].transparent_zatoshi, 20_000_000);
    assert_eq!(accounts[0].ironwood_zatoshi, 100_000_000);

    // Model the rows retained by the SDK after a transaction is orphaned. Pool-local
    // output ids deliberately overlap, so spending one pool must not spend another.
    let db = rusqlite::Connection::open(&path).unwrap();
    let height = u32::from(
        st.wallet()
            .block_fully_scanned()
            .unwrap()
            .unwrap()
            .block_height(),
    );
    db.execute(
        "INSERT INTO transactions(txid, min_observed_height, expiry_height)
         VALUES (zeroblob(32), 0, ?1)",
        [height + 40],
    )
    .unwrap();
    let tx = db.last_insert_rowid();
    db.execute(
        "INSERT INTO ironwood_received_notes(
            transaction_id, action_index, account_id, diversifier, value, rho, rseed,
            is_change, note_version)
         SELECT ?1, 0, account_id, diversifier, 10_000_000, rho, rseed, 1, note_version
         FROM ironwood_received_notes WHERE id = 1",
        [tx],
    )
    .unwrap();
    db.execute(
        "INSERT INTO transparent_received_outputs(
            transaction_id, output_index, account_id, address, script, value_zat, address_id)
         SELECT ?1, 0, account_id, address, script, 5_000_000, address_id
         FROM transparent_received_outputs WHERE id = 1",
        [tx],
    )
    .unwrap();
    db.execute(
        "INSERT INTO ironwood_received_note_spends VALUES (1, ?1)",
        [tx],
    )
    .unwrap();
    for mined in [None, Some(height + 1), Some(height), None] {
        db.execute(
            "UPDATE transactions SET mined_height = ?1 WHERE id_tx = ?2",
            rusqlite::params![mined, tx],
        )
        .unwrap();
        wallet.apply_balances(&mut accounts).await.unwrap();
        let confirmed = mined == Some(height);
        assert_eq!(
            accounts[0].ironwood_zatoshi,
            if confirmed { 10_000_000 } else { 100_000_000 }
        );
        assert_eq!(
            accounts[0].transparent_zatoshi,
            if confirmed { 25_000_000 } else { 20_000_000 }
        );
    }
    db.execute(
        "INSERT INTO transparent_received_output_spends VALUES (1, ?1)",
        [tx],
    )
    .unwrap();
    for mined in [Some(height), None] {
        db.execute(
            "UPDATE transactions SET mined_height = ?1 WHERE id_tx = ?2",
            rusqlite::params![mined, tx],
        )
        .unwrap();
        wallet.apply_balances(&mut accounts).await.unwrap();
        assert_eq!(
            accounts[0].transparent_zatoshi,
            if mined.is_some() {
                5_000_000
            } else {
                20_000_000
            }
        );
    }
    let retained: (u32, u32, u32) = db
        .query_row(
            "SELECT (SELECT COUNT(*) FROM ironwood_received_note_spends),
                (SELECT COUNT(*) FROM transparent_received_output_spends),
                (SELECT COUNT(*) FROM v_received_outputs WHERE transaction_id = ?1)",
            [tx],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(retained, (1, 1, 2));
}

#[test]
fn ironwood_quote_reports_fee_and_max() {
    let mut st = test_state();
    fund_ironwood(&mut st, 100_000_000);
    let quote = quote(&mut st, "ironwood", "ironwood").unwrap();
    assert_eq!(quote.available_zatoshi, 100_000_000);
    assert_eq!(quote.fee_zatoshi, 10_000);
    assert_eq!(quote.max_zatoshi, 99_990_000);
}

#[test]
fn ironwood_quote_max_is_zero_when_balance_only_covers_the_fee() {
    let mut st = test_state();
    fund_ironwood(&mut st, 10_000);
    let quote = quote(&mut st, "ironwood", "ironwood").unwrap();
    assert_eq!(quote.available_zatoshi, 10_000);
    assert_eq!(quote.fee_zatoshi, 10_000);
    assert_eq!(quote.max_zatoshi, 0);
}

#[test]
fn ironwood_quote_max_is_zero_when_balance_is_below_the_fee() {
    let mut st = test_state();
    fund_ironwood(&mut st, 9_000);
    let quote = quote(&mut st, "ironwood", "ironwood").unwrap();
    assert_eq!(quote.available_zatoshi, 9_000);
    assert_eq!(quote.fee_zatoshi, 10_000);
    assert_eq!(quote.max_zatoshi, 0);
}

#[test]
fn ironwood_quote_reports_zero_max_for_an_empty_account() {
    let mut st = test_state();
    add_empty_blocks(&mut st, 10);
    let quote = quote(&mut st, "ironwood", "ironwood").unwrap();
    assert_eq!(quote.available_zatoshi, 0);
    assert_eq!(quote.fee_zatoshi, 10_000);
    assert_eq!(quote.max_zatoshi, 0);
}

// ZIP-317: max(⌈148/150⌉, 0) transparent + 2 Ironwood actions (recipient,
// change) = 3 logical actions, above the 2-action grace → 3 × 5_000.
#[test]
fn transparent_quote_reports_fee_and_max() {
    let mut st = test_state();
    fund_transparent(&mut st, 100_000_000);
    let quote = quote(&mut st, "transparent", "ironwood").unwrap();
    assert_eq!(quote.available_zatoshi, 100_000_000);
    assert_eq!(quote.fee_zatoshi, 15_000);
    assert_eq!(quote.max_zatoshi, 99_985_000);
}

// ZIP-317: max(⌈148/150⌉, ⌈34/34⌉) transparent + 1 Ironwood change action
// = 2 logical actions, inside the 2-action grace → 2 × 5_000.
#[test]
fn transparent_quote_to_transparent_reports_fee_and_max() {
    let mut st = test_state();
    fund_transparent(&mut st, 100_000_000);
    let quote = quote(&mut st, "transparent", "transparent").unwrap();
    assert_eq!(quote.available_zatoshi, 100_000_000);
    assert_eq!(quote.fee_zatoshi, 10_000);
    assert_eq!(quote.max_zatoshi, 99_990_000);
}

#[test]
fn transparent_quote_max_is_zero_when_balance_is_below_the_fee() {
    let mut st = test_state();
    fund_transparent(&mut st, 9_000);
    let quote = quote(&mut st, "transparent", "ironwood").unwrap();
    assert_eq!(quote.available_zatoshi, 9_000);
    assert_eq!(quote.fee_zatoshi, 15_000);
    assert_eq!(quote.max_zatoshi, 0);
}

#[test]
fn transparent_quote_reports_zero_max_for_dust() {
    let mut st = test_state();
    fund_transparent(&mut st, 5_000);
    let quote = quote(&mut st, "transparent", "ironwood").unwrap();
    assert_eq!(quote.available_zatoshi, 0);
    // No spendable input, so only the two Ironwood actions pay: the
    // 2-action grace floor still applies once an input exists.
    assert_eq!(quote.fee_zatoshi, 10_000);
    assert_eq!(quote.max_zatoshi, 0);
}

#[test]
fn quote_reports_scan_required_before_any_blocks_are_scanned() {
    let mut st = test_state();
    for (source_pool, destination_pool) in [("ironwood", "ironwood"), ("transparent", "ironwood")] {
        let error = quote(&mut st, source_pool, destination_pool)
            .err()
            .expect("quote must fail before any blocks are scanned");
        assert!(
            matches!(error.downcast_ref(), Some(PaymentError::ScanRequired)),
            "{source_pool}->{destination_pool}: {error:#}"
        );
    }
}

#[tokio::test]
async fn scanned_expiry_does_not_authorize_replacing_a_journal() {
    let test = test_wallet();
    rusqlite::Connection::open(test.path.join("wallet.db")).unwrap().execute("INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES('activity-1','original-txid',x'010203',140)", []).unwrap();
    test.wallet.update_chain_tip(140).await.unwrap();
    let prepared = test
        .wallet
        .prepare(
            Some("activity-1"),
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
    assert_eq!(prepared.txid, "original-txid");
    assert_eq!(prepared.raw_transaction, [1, 2, 3]);
}

#[tokio::test]
async fn replacement_authorization_requires_the_original_journal() {
    let test = test_wallet();
    let error = test
        .wallet
        .prepare(
            Some("missing"),
            Some("expired-txid"),
            "invalid seed",
            0,
            "invalid pool",
            "invalid address",
            0,
            None,
            Ok,
        )
        .await
        .err()
        .expect("replacement must fail");
    assert!(
        error
            .to_string()
            .contains("replacement authorization has no journal"),
        "{error:#}"
    );
    assert!(test.wallet.prepared("missing").await.unwrap().is_none());
}

#[tokio::test]
async fn replacement_authorization_rechecks_the_journal_inside_the_worker() {
    let test = test_wallet();
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let holder = {
        let wallet = test.wallet.clone();
        tokio::spawn(async move {
            wallet.with_db(move |db| {
                    entered.send(()).unwrap();
                    release_rx.recv().unwrap();
                    db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
                        ext.execute("INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES('activity-1','newer-txid',x'040506',140)", [])?;
                        Ok(())
                    })
                }).await
        })
    };
    entered_rx.await.unwrap();
    let preparing = {
        let wallet = test.wallet.clone();
        tokio::spawn(async move {
            wallet
                .prepare(
                    Some("activity-1"),
                    Some("expired-txid"),
                    "invalid seed",
                    0,
                    "invalid pool",
                    "invalid address",
                    0,
                    None,
                    Ok,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(!preparing.is_finished());
    release.send(()).unwrap();
    holder.await.unwrap().unwrap();
    let prepared = preparing.await.unwrap().unwrap();
    assert_eq!(prepared.txid, "newer-txid");
    assert_eq!(prepared.raw_transaction, [4, 5, 6]);
    assert_eq!(
        test.wallet
            .prepared("activity-1")
            .await
            .unwrap()
            .unwrap()
            .txid,
        "newer-txid"
    );
}

#[tokio::test]
async fn matching_replacement_authorization_keeps_the_journal_after_proposal_failure() {
    let test = test_wallet();
    rusqlite::Connection::open(test.path.join("wallet.db")).unwrap().execute("INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES('activity-1','expired-txid',x'010203',140)", []).unwrap();
    let error = test
        .wallet
        .prepare(
            Some("activity-1"),
            Some("expired-txid"),
            "invalid seed",
            0,
            "invalid pool",
            "invalid address",
            0,
            None,
            Ok,
        )
        .await
        .err()
        .expect("replacement must fail");
    assert_eq!(error.to_string(), "invalid source account");
    let journal = test.wallet.prepared("activity-1").await.unwrap().unwrap();
    assert_eq!(journal.txid, "expired-txid");
    assert_eq!(journal.raw_transaction, [1, 2, 3]);
}

#[tokio::test]
async fn shielding_scanned_expiry_preserves_retained_signed_bytes_without_authorization() {
    let test = test_wallet();
    test.wallet
            .with_db(|db| {
                db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
                    ext.execute(
                        "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES(?1,'shielding-original',x'010203',140)",
                        [TREASURY_SHIELDING_JOURNAL_ID],
                    )?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    test.wallet.update_chain_tip(141).await.unwrap();
    let prepared = test
        .wallet
        .prepare_shield_coinbase(
            "invalid seed",
            crate::db::TREASURY_ACCOUNT_ID,
            "invalid address",
            "invalid address",
            1,
            None,
        )
        .await
        .unwrap();
    assert_eq!(prepared.txid, "shielding-original");
    assert_eq!(prepared.raw_transaction, [1, 2, 3]);
    assert_eq!(prepared.expiry_height, 140);
    let retained = test.wallet.prepared_shielding().await.unwrap().unwrap();
    assert_eq!(retained.txid, prepared.txid);
    assert_eq!(retained.raw_transaction, prepared.raw_transaction);
    assert_eq!(retained.expiry_height, prepared.expiry_height);
}

#[tokio::test]
async fn shielding_stale_replacement_authorization_returns_the_newer_journal() {
    let test = test_wallet();
    test.wallet
            .with_db(|db| {
                db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
                    ext.execute(
                        "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES(?1,'shielding-newer',x'040506',180)",
                        [TREASURY_SHIELDING_JOURNAL_ID],
                    )?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    let prepared = test
        .wallet
        .prepare_shield_coinbase(
            "invalid seed",
            crate::db::TREASURY_ACCOUNT_ID,
            "invalid address",
            "invalid address",
            1,
            Some("shielding-expired"),
        )
        .await
        .unwrap();
    assert_eq!(prepared.txid, "shielding-newer");
    assert_eq!(prepared.raw_transaction, [4, 5, 6]);
    assert_eq!(prepared.expiry_height, 180);
    let retained = test.wallet.prepared_shielding().await.unwrap().unwrap();
    assert_eq!(retained.txid, prepared.txid);
    assert_eq!(retained.raw_transaction, prepared.raw_transaction);
    assert_eq!(retained.expiry_height, prepared.expiry_height);
}
