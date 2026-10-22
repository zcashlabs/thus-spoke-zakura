use crate::wallet::tests::test_wallet;
use transparent::{
    bundle::{OutPoint, TxOut},
    keys::TransparentKeyScope,
};
use zcash_client_backend::{
    data_api::testing::{
        self, AddressType, IronwoodFvk, TestBuilder, orchard::OrchardPoolTester,
        pool::ShieldedPoolTester, sapling::SaplingPoolTester,
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

fn fund_sapling(st: &mut TestState, value: u64) {
    let fvk = SaplingPoolTester::test_account_fvk(st);
    let (height, _, _) = st.generate_next_block(
        &fvk,
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(value),
    );
    st.scan_cached_blocks(height, 1);
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
        wallet_path: path.clone(),
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
fn ironwood_send_rejects_sapling_funds_when_ironwood_is_empty_or_insufficient() {
    for ironwood in [0, 1_000_000] {
        let mut st = test_state();
        if ironwood != 0 {
            fund_ironwood(&mut st, ironwood);
        }
        fund_sapling(&mut st, 10_000_000);
        for destination in ["ironwood", "transparent"] {
            let quoted = quote(&mut st, "ironwood", destination).unwrap();
            assert_eq!(quoted.available_zatoshi, ironwood);
            let recipient = recipient(&st, destination);
            let account_id = st.test_account().unwrap().id();
            let params = *st.network();
            let error = RealWallet::propose_send(
                st.wallet_mut(),
                &params,
                account_id,
                "ironwood",
                &recipient,
                Zatoshis::const_from_u64(5_000_000),
                None,
            )
            .expect_err("Sapling must not cover an Ironwood shortfall");
            assert!(
                matches!(error.downcast_ref(), Some(PaymentError::InsufficientFunds { available, .. }) if *available == ironwood),
                "{ironwood} Ironwood -> {destination}: {error:#}"
            );
        }
    }
}

#[test]
fn ironwood_send_uses_only_ironwood_inputs_with_both_pools_funded() {
    let mut st = test_state();
    fund_ironwood(&mut st, 10_000_000);
    fund_sapling(&mut st, 10_000_000);
    for destination in ["ironwood", "transparent"] {
        let recipient = recipient(&st, destination);
        let account_id = st.test_account().unwrap().id();
        let params = *st.network();
        let proposal = RealWallet::propose_send(
            st.wallet_mut(),
            &params,
            account_id,
            "ironwood",
            &recipient,
            Zatoshis::const_from_u64(5_000_000),
            None,
        )
        .unwrap();
        let notes: Vec<_> = proposal
            .steps()
            .iter()
            .flat_map(|step| {
                step.shielded_inputs()
                    .into_iter()
                    .flat_map(|inputs| inputs.notes())
            })
            .collect();
        assert!(!notes.is_empty());
        assert!(
            notes
                .iter()
                .all(|note| note.note().pool() == ShieldedPool::Ironwood)
        );
        assert!(
            proposal
                .steps()
                .iter()
                .all(|step| step.transparent_inputs().is_empty())
        );
    }
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

// A fixed send gathers at most 1,333 transparent inputs. With one more 100,000-zatoshi UTXO
// than that, the quoted maximum must still be an amount Send can propose.
#[test]
fn transparent_quote_max_is_sendable_when_utxos_exceed_the_input_cap() {
    let mut st = test_state();
    fund_transparent(&mut st, 100_000);
    st.wallet()
        .conn()
        .execute(
            "WITH RECURSIVE n(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM n WHERE value < 1333)
             INSERT INTO transparent_received_outputs(
                transaction_id, output_index, account_id, address, script, value_zat, address_id)
             SELECT transaction_id, n.value, account_id, address, script, value_zat, address_id
             FROM transparent_received_outputs, n
             WHERE id = 1",
            [],
        )
        .unwrap();

    let quote = quote(&mut st, "transparent", "ironwood").unwrap();
    assert_eq!(MAX_TRANSPARENT_INPUTS, 1_333);
    // 1,333 transparent + 2 Ironwood logical actions at 5,000 zatoshis.
    assert_eq!(quote.fee_zatoshi, 6_675_000);
    assert_eq!(quote.available_zatoshi, 133_300_000);
    assert_eq!(quote.max_zatoshi, 126_625_000);

    let recipient = recipient(&st, "ironwood");
    let account_id = st.test_account().unwrap().id();
    let params = *st.network();
    let mut propose = |amount| {
        RealWallet::propose_send(
            st.wallet_mut(),
            &params,
            account_id,
            "transparent",
            &recipient,
            Zatoshis::from_u64(amount).unwrap(),
            None,
        )
    };
    propose(quote.max_zatoshi).unwrap();
    assert!(propose(quote.max_zatoshi + 1).is_err());
}

/// Copies the harness wallet into a file, as the server opens it, for the
/// accounts given in development-account order.
fn file_wallet(
    st: &TestState,
    dir: &std::path::Path,
    name: &str,
    account_ids: Vec<<TestDb as WalletRead>::AccountId>,
) -> RealWallet {
    let path = dir.join(name);
    st.wallet()
        .conn()
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    RealWallet {
        db: Arc::new(Mutex::new(
            WalletDb::for_path(&path, *st.network(), SystemClock, UnwrapErr(SysRng))
                .unwrap()
                .with_enhancement_mode(EnhancementMode::Standard),
        )),
        account_ids,
        lightwalletd: String::new(),
        wallet_path: path,
    }
}

/// Account 1 sends 0.1 ZEC with `memo` to a second account under the same
/// seed. Returns the second account and the unmined transaction.
fn send_to_second_account(
    st: &mut TestState,
    memo: &str,
) -> (<TestDb as WalletRead>::AccountId, Transaction) {
    let second = st.create_account_from_test_seed("second").0;
    fund_ironwood(st, 100_000_000);
    let address = st
        .wallet()
        .get_last_generated_address_matching(second, UnifiedAddressRequest::AllAvailableKeys)
        .unwrap()
        .unwrap();
    let account_id = st.test_account().unwrap().id();
    let usk = st.test_account().unwrap().usk().clone();
    let params = *st.network();
    let proposal = RealWallet::propose_send(
        st.wallet_mut(),
        &params,
        account_id,
        "ironwood",
        &Address::Unified(address),
        Zatoshis::const_from_u64(10_000_000),
        Some(MemoBytes::from_bytes(memo.as_bytes()).unwrap()),
    )
    .unwrap();
    let txid = *st
        .create_proposed_transactions::<Infallible, _, Infallible, _>(
            &usk,
            OvkPolicy::Sender,
            &proposal,
        )
        .unwrap()
        .first();
    let tx = st.wallet().get_transaction(txid).unwrap().unwrap();
    (second, tx)
}

/// A node that knows one transaction, mined at `height`.
async fn node_knowing(txid: String, height: u32) -> String {
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    let rpc = Router::new().route(
        "/",
        post(move |Json(req): Json<Value>| {
            let txid = txid.clone();
            async move {
                Json(
                    if req["method"] == "getrawtransaction" && req["params"][0] == txid {
                        json!({
                            "result": {"txid": txid, "height": height, "confirmations": 1},
                            "error": null,
                            "id": req["id"]
                        })
                    } else {
                        json!({
                            "result": null,
                            "error": {"code": -5, "message": "No such transaction"},
                            "id": req["id"]
                        })
                    },
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, rpc).await });
    url
}

#[tokio::test]
async fn wallet_view_shows_the_memo_change_and_fee_of_a_wallet_built_send() {
    let mut st = test_state();
    let (second, tx) = send_to_second_account(&mut st, "order 42");
    let first = st.test_account().unwrap().id();
    let dir = tempfile::tempdir().unwrap();

    // Before the block is scanned the SDK has not linked the payment to the
    // second account; the recorded address still names it.
    let unmined = file_wallet(&st, dir.path(), "unmined.db", vec![first, second]);
    let view = unmined.wallet_transaction(tx.txid()).await.unwrap();
    assert_eq!(
        view.details,
        crate::wallet::history::TransactionDetails::Complete
    );
    let payment = view.outputs.iter().find(|output| !output.change).unwrap();
    assert_eq!(payment.to, Some(crate::wallet::history::Party::Account(2)));
    assert_eq!(
        payment.memo,
        Some(crate::wallet::history::StoredMemo::Text(
            "order 42".to_owned()
        ))
    );

    let (height, _) = st.generate_next_block_including(tx.txid());
    st.scan_cached_blocks(height, 1);
    let wallet = file_wallet(&st, dir.path(), "wallet.db", vec![first, second]);
    let store = crate::db::Store::open(dir.path().join("ths.db")).unwrap();
    store.initialize().unwrap();
    let txid = tx.txid().to_string();
    let node = node_knowing(txid.clone(), u32::from(height)).await;
    let state = crate::state::AppState::new(store, wallet, node, "test".into());

    let (status, view) =
        crate::state::tests::api_get(&state, &format!("/api/v1/transactions/{txid}/wallet")).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["txid"], txid);
    assert_eq!(view["status"], "confirmed");
    assert_eq!(view["mined_height"], u32::from(height));
    assert_eq!(view["confirmations"], 1);
    assert_eq!(view["details"], "complete");
    assert_eq!(view["fee_zatoshi"], 10_000);
    let outputs = view["outputs"].as_array().unwrap();
    let payment = outputs.iter().find(|output| output["to"] == 2).unwrap();
    assert_eq!(payment["from"], 1);
    assert_eq!(payment["pool"], "ironwood");
    assert_eq!(payment["value_zatoshi"], 10_000_000);
    assert_eq!(payment["change"], false);
    assert_eq!(payment["memo"], serde_json::json!({"text": "order 42"}));
    assert!(
        payment["to_address"]
            .as_str()
            .unwrap()
            .starts_with("uregtest1")
    );
    let change = outputs.iter().find(|output| output["to"] == 1).unwrap();
    assert_eq!(change["from"], 1);
    assert_eq!(change["change"], true);
    assert_eq!(change["value_zatoshi"], 89_990_000);
    assert_eq!(change["memo"], serde_json::Value::Null);
    assert_eq!(change["to_address"], serde_json::Value::Null);
    assert_eq!(outputs.len(), 2);
    assert_eq!(
        view["spends"],
        serde_json::json!([{"account": 1, "pool": "ironwood", "value_zatoshi": 100_000_000}])
    );

    let (status, receiver) = crate::state::tests::api_get(
        &state,
        &format!("/api/v1/transactions/{txid}/wallet?account=2"),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(receiver["outputs"].as_array().unwrap().len(), 1);
    assert_eq!(receiver["outputs"][0]["to"], 2);
    assert_eq!(receiver["spends"], serde_json::json!([]));

    let (status, uninvolved) = crate::state::tests::api_get(
        &state,
        &format!("/api/v1/transactions/{txid}/wallet?account=3"),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(uninvolved["outputs"], serde_json::json!([]));
    assert_eq!(uninvolved["spends"], serde_json::json!([]));

    let (status, _) = crate::state::tests::api_get(
        &state,
        &format!("/api/v1/transactions/{txid}/wallet?account=6"),
    )
    .await;
    assert_eq!(status, 400);
    let (status, _) = crate::state::tests::api_get(
        &state,
        &format!("/api/v1/transactions/{}/wallet", "ab".repeat(32)),
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn wallet_view_details_follow_transaction_enhancement() {
    let mut sender = test_state();
    let (_, tx) = send_to_second_account(&mut sender, "order 42");

    // A second wallet under the same seed finds the payment only by scanning
    // a compact block, which carries no memo.
    let mut st = test_state();
    let second = st.create_account_from_test_seed("second").0;
    let (height, _) = st.generate_next_block_from_tx(1, &tx);
    st.scan_cached_blocks(height, 1);
    let first = st.test_account().unwrap().id();
    let dir = tempfile::tempdir().unwrap();
    let txid = tx.txid();
    let received =
        |view: &crate::wallet::history::WalletTransaction| view.clone().for_account(2).outputs;

    let enhanced = file_wallet(&st, dir.path(), "enhanced.db", vec![first, second]);
    let view = enhanced.wallet_transaction(txid).await.unwrap();
    assert_eq!(
        view.details,
        crate::wallet::history::TransactionDetails::Pending
    );
    let outputs = received(&view);
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].value_zatoshi, 10_000_000);
    assert_eq!(outputs[0].memo, None);

    let mut raw = Vec::new();
    tx.write(&mut raw).unwrap();
    enhanced
        .enhance_transaction(txid, raw, u32::from(height))
        .await
        .unwrap();
    let view = enhanced.wallet_transaction(txid).await.unwrap();
    assert_eq!(
        view.details,
        crate::wallet::history::TransactionDetails::Complete
    );
    assert_eq!(
        received(&view)[0].memo,
        Some(crate::wallet::history::StoredMemo::Text(
            "order 42".to_owned()
        ))
    );

    // The SDK keeps an Ironwood transaction's request when lightwalletd cannot
    // supply it, so its details stay pending rather than unavailable.
    let missing = file_wallet(&st, dir.path(), "missing.db", vec![first, second]);
    missing
        .transaction_enhancement_not_found(txid)
        .await
        .unwrap();
    let view = missing.wallet_transaction(txid).await.unwrap();
    assert_eq!(
        view.details,
        crate::wallet::history::TransactionDetails::Pending
    );
    assert_eq!(received(&view)[0].memo, None);
}

#[tokio::test]
async fn wallet_view_details_are_unavailable_once_enhancement_is_abandoned() {
    // A Sapling payment has no Ironwood routing, so "not found" retires its
    // request and the memo is never recovered.
    let mut sender = test_state();
    let second = sender.create_account_from_test_seed("second").0;
    fund_sapling(&mut sender, 100_000_000);
    let address = sender
        .wallet()
        .get_last_generated_address_matching(second, UnifiedAddressRequest::AllAvailableKeys)
        .unwrap()
        .unwrap();
    let account_id = sender.test_account().unwrap().id();
    let usk = sender.test_account().unwrap().usk().clone();
    let proposal = sender
        .propose_standard_transfer::<Infallible>(
            account_id,
            StandardFeeRule::Zip317,
            ConfirmationsPolicy::MIN,
            &Address::Sapling(*address.sapling().unwrap()),
            Zatoshis::const_from_u64(10_000_000),
            Some(MemoBytes::from_bytes(b"order 42").unwrap()),
            None,
            ShieldedPool::Sapling,
        )
        .unwrap();
    let txid = *sender
        .create_proposed_transactions::<Infallible, _, Infallible, _>(
            &usk,
            OvkPolicy::Sender,
            &proposal,
        )
        .unwrap()
        .first();
    let tx = sender.wallet().get_transaction(txid).unwrap().unwrap();

    let mut st = test_state();
    let second = st.create_account_from_test_seed("second").0;
    let (height, _) = st.generate_next_block_from_tx(1, &tx);
    st.scan_cached_blocks(height, 1);
    let first = st.test_account().unwrap().id();
    let dir = tempfile::tempdir().unwrap();
    let wallet = file_wallet(&st, dir.path(), "wallet.db", vec![first, second]);
    let view = wallet.wallet_transaction(txid).await.unwrap();
    assert_eq!(
        view.details,
        crate::wallet::history::TransactionDetails::Pending
    );

    wallet
        .transaction_enhancement_not_found(txid)
        .await
        .unwrap();
    let view = wallet
        .wallet_transaction(txid)
        .await
        .unwrap()
        .for_account(2);
    assert_eq!(
        view.details,
        crate::wallet::history::TransactionDetails::Unavailable
    );
    assert_eq!(view.outputs.len(), 1);
    assert_eq!(view.outputs[0].pool, "sapling");
    assert_eq!(view.outputs[0].memo, None);
}
