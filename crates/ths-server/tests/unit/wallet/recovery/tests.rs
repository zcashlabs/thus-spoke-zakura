use transparent::bundle::{Authorized as TransparentAuthorized, Bundle, TxIn};
use zcash_primitives::transaction::{Authorized, TransactionData, TxVersion};
use zcash_script::script::Evaluable;

use super::*;
use crate::db::{Store, TREASURY_ACCOUNT_ID};

fn setup() -> (tempfile::TempDir, Store, RealWallet) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    (dir, store, wallet)
}

fn transaction(receiver: &str, prevout: OutPoint, tag: u32) -> Transaction {
    let Address::Transparent(address) = Address::decode(&regtest_network(), receiver).unwrap()
    else {
        panic!("transparent receiver required")
    };
    TransactionData::<Authorized>::from_parts(
        TxVersion::V5,
        BranchId::for_height(&regtest_network(), 2.into()),
        tag,
        200.into(),
        Some(Bundle {
            vin: vec![TxIn::from_parts(
                prevout,
                Script(script::Code(vec![1, 2])),
                u32::MAX,
            )],
            vout: vec![TxOut::new(
                Zatoshis::from_u64(100_000_000).unwrap(),
                Script(script::Code(address.script().to_bytes())),
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

fn raw(tx: &Transaction) -> String {
    let mut bytes = Vec::new();
    tx.write(&mut bytes).unwrap();
    hex::encode(bytes)
}

fn checkpoint(height: u64, byte: u8) -> ChainCheckpoint {
    ChainCheckpoint {
        height,
        hash: hex::encode([byte; 32]),
    }
}

#[tokio::test]
async fn cursor_at_birthday_floor_can_rewind_before_the_first_scan() {
    let (_dir, store, wallet) = setup();
    let receiver = store
        .account(TREASURY_ACCOUNT_ID)
        .unwrap()
        .transparent_address;
    wallet
        .initialize_treasury_cursor(&receiver, 1, &"01".repeat(32))
        .await
        .unwrap();
    assert!(wallet.scanned_checkpoint().await.unwrap().is_none());

    wallet
        .rewind_to_height(ChainState::empty(1.into(), BlockHash([2; 32])))
        .await
        .unwrap();
    let cursor = wallet.treasury_cursor().await.unwrap().unwrap();
    assert_eq!(cursor.height, 1);
    assert_eq!(cursor.block_hash, "02".repeat(32));
}

#[tokio::test]
async fn failed_cursor_commit_rolls_back_reward_import_and_reopen_keeps_progress() {
    let (dir, store, wallet) = setup();
    let receiver = store
        .account(TREASURY_ACCOUNT_ID)
        .unwrap()
        .transparent_address;
    let initial = wallet
        .initialize_treasury_cursor(&receiver, 1, &"01".repeat(32))
        .await
        .unwrap();
    let reward = transaction(&receiver, OutPoint::new([0; 32], u32::MAX), 0);
    let txid = reward.txid();
    let connection = rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER ext_ths_fail_cursor BEFORE UPDATE ON ext_tsz_treasury_cursor BEGIN SELECT RAISE(ABORT, 'injected cursor failure'); END").unwrap();

    let error = wallet
        .commit_treasury_discovery(&initial, &checkpoint(2, 2), Some(&raw(&reward)), &[])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("injected cursor failure"));
    assert_eq!(
        wallet.treasury_cursor().await.unwrap(),
        Some(initial.clone())
    );
    assert!(
        wallet
            .with_db(move |db| Ok(db.get_transaction(txid)?.is_none()))
            .await
            .unwrap()
    );

    connection
        .execute_batch("DROP TRIGGER ext_ths_fail_cursor")
        .unwrap();
    let advanced = wallet
        .commit_treasury_discovery(&initial, &checkpoint(2, 2), Some(&raw(&reward)), &[])
        .await
        .unwrap();
    drop(wallet);
    let reopened = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    assert_eq!(reopened.treasury_cursor().await.unwrap(), Some(advanced));
    assert!(
        reopened
            .with_db(move |db| Ok(db.get_transaction(txid)?.is_some()))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn known_spender_must_be_stored_and_spend_the_exact_reward_output() {
    let (_dir, store, wallet) = setup();
    let receiver = store
        .account(TREASURY_ACCOUNT_ID)
        .unwrap()
        .transparent_address;
    let reward = transaction(&receiver, OutPoint::new([0; 32], u32::MAX), 0);
    wallet
        .enhance_transaction(reward.txid(), hex::decode(raw(&reward)).unwrap(), 2)
        .await
        .unwrap();
    let spender = transaction(&receiver, OutPoint::new(*reward.txid().as_ref(), 0), 1);
    wallet
        .enhance_transaction(spender.txid(), hex::decode(raw(&spender)).unwrap(), 103)
        .await
        .unwrap();
    let spenders = wallet
        .known_spending_transactions(&reward.txid().to_string(), 0)
        .await
        .unwrap();
    assert_eq!(spenders, vec![spender.txid().to_string()]);
    assert!(
        wallet
            .known_spending_transactions(&reward.txid().to_string(), 1)
            .await
            .unwrap()
            .is_empty()
    );

    let initial = wallet
        .initialize_treasury_cursor(&receiver, 1, &"01".repeat(32))
        .await
        .unwrap();
    let error = wallet
        .commit_treasury_discovery(
            &initial,
            &checkpoint(2, 2),
            Some(&raw(&reward)),
            &[(0, "09".repeat(32))],
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("spender is not stored"));
    assert_eq!(
        wallet.treasury_cursor().await.unwrap(),
        Some(initial.clone())
    );
    let advanced = wallet
        .commit_treasury_discovery(
            &initial,
            &checkpoint(2, 2),
            Some(&raw(&reward)),
            &[(0, spenders[0].clone())],
        )
        .await
        .unwrap();
    assert_eq!(advanced.height, 2);
}

#[tokio::test]
async fn rewind_updates_sdk_and_cursor_atomically() {
    let (dir, store, wallet) = setup();
    let receiver = store
        .account(TREASURY_ACCOUNT_ID)
        .unwrap()
        .transparent_address;
    use zcash_client_backend::data_api::scanning::ScanPriority;
    wallet.update_chain_tip(2).await.unwrap();
    wallet
        .scan_batch(
            ScanRange::from_parts(2.into()..3.into(), ScanPriority::Historic),
            vec![CompactBlock {
                height: 2,
                hash: vec![2; 32],
                prev_hash: vec![1; 32],
                chain_metadata: Some(Default::default()),
                ..Default::default()
            }],
            ChainState::empty(1.into(), BlockHash([1; 32])),
        )
        .await
        .unwrap();
    let initial = wallet
        .initialize_treasury_cursor(&receiver, 1, &"01".repeat(32))
        .await
        .unwrap();
    let reward = transaction(&receiver, OutPoint::new([0; 32], u32::MAX), 0);
    let txid = reward.txid();
    let advanced = wallet
        .commit_treasury_discovery(&initial, &checkpoint(2, 2), Some(&raw(&reward)), &[])
        .await
        .unwrap();
    assert_eq!(
        wallet
            .with_db(move |db| Ok(db.get_tx_height(txid)?))
            .await
            .unwrap(),
        Some(2.into())
    );
    let connection = rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER ext_ths_fail_rewind BEFORE UPDATE ON ext_tsz_treasury_cursor BEGIN SELECT RAISE(ABORT, 'injected rewind failure'); END").unwrap();
    let state = ChainState::empty(1.into(), BlockHash([1; 32]));
    let error = wallet.rewind_to_height(state.clone()).await.unwrap_err();
    assert!(error.to_string().contains("injected rewind failure"));
    assert_eq!(wallet.treasury_cursor().await.unwrap(), Some(advanced));
    assert_eq!(
        wallet
            .with_db(move |db| Ok(db.get_tx_height(txid)?))
            .await
            .unwrap(),
        Some(2.into())
    );

    connection
        .execute_batch("DROP TRIGGER ext_ths_fail_rewind")
        .unwrap();
    wallet.rewind_to_height(state).await.unwrap();
    let cursor = wallet.treasury_cursor().await.unwrap().unwrap();
    assert_eq!(cursor.height, 1);
    assert_eq!(cursor.block_hash, "01".repeat(32));
    assert_eq!(
        wallet
            .with_db(move |db| Ok(db.get_tx_height(txid)?))
            .await
            .unwrap(),
        None
    );
}
