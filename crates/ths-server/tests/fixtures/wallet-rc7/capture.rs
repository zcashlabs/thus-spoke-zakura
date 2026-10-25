use anyhow::Result;
use secrecy::ExposeSecret;
use transparent::{
    bundle::{OutPoint, TxOut},
    keys::TransparentKeyScope,
};
use zcash_client_backend::{
    data_api::{
        Account as _, WalletRead,
        testing::{
            AddressType, IronwoodFvk, TestBuilder, orchard::OrchardPoolTester,
            pool::ShieldedPoolTester,
        },
    },
    wallet::WalletTransparentOutput,
};
use zcash_client_sqlite::testing::{BlockCache, db::TestDbFactory};
use zcash_keys::keys::UnifiedAddressRequest;
use zcash_primitives::block::BlockHash;
use zcash_protocol::{consensus::BlockHeight, local_consensus::LocalNetwork, value::Zatoshis};

fn main() -> Result<()> {
    let destination =
        std::path::PathBuf::from(std::env::args_os().nth(1).expect("capture directory"));
    std::fs::create_dir_all(&destination)?;
    let one = Some(BlockHeight::from_u32(1));
    let network = LocalNetwork {
        overwinter: one,
        sapling: one,
        blossom: one,
        heartwood: one,
        canopy: one,
        nu5: one,
        nu6: one,
        nu6_1: one,
        nu6_2: one,
        nu6_3: one,
        nu7: one,
    };
    let mut state = TestBuilder::new()
        .with_network(network)
        .with_data_store_factory(TestDbFactory::default())
        .with_block_cache(BlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build();
    for id in 2..=6 {
        state.create_account_from_test_seed(&format!("Account {id}"));
    }
    state
        .wallet()
        .conn()
        .execute("UPDATE accounts SET name='Account 1' WHERE name=''", [])?;
    let fvk = IronwoodFvk(OrchardPoolTester::test_account_fvk(&state));
    let (height, _, _) = state.generate_next_block(
        &fvk,
        AddressType::DefaultExternal,
        Zatoshis::const_from_u64(100_000_000),
    );
    state.scan_cached_blocks(height, 1);
    for _ in 0..6 {
        let (height, _) = state.generate_empty_block();
        state.scan_cached_blocks(height, 1);
    }
    let account = state.test_account().unwrap().id();
    let address = *state
        .wallet()
        .get_last_generated_address_matching(account, UnifiedAddressRequest::AllAvailableKeys)?
        .unwrap()
        .transparent()
        .unwrap();
    let utxo = WalletTransparentOutput::from_parts(
        OutPoint::new([1; 32], 0),
        TxOut::new(
            Zatoshis::const_from_u64(20_000_000),
            address.script().into(),
        ),
        state.wallet().chain_height()?,
        Some(account),
        Some(TransparentKeyScope::EXTERNAL),
        None,
    )
    .unwrap();
    use zcash_client_backend::data_api::WalletWrite;
    state.wallet_mut().put_received_transparent_utxo(&utxo)?;
    state.wallet().conn().execute(
        "VACUUM INTO ?1",
        [destination.join("wallet.db").to_str().unwrap()],
    )?;
    let app = rusqlite::Connection::open(destination.join("ths.db"))?;
    app.execute_batch("CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL)")?;
    app.execute(
        "INSERT INTO metadata VALUES('seed',?1)",
        [hex::encode(state.test_seed().unwrap().expose_secret())],
    )?;
    app.execute(
        "INSERT INTO metadata VALUES('mnemonic','fixture-only SDK seed; not a BIP39 mnemonic')",
        [],
    )?;
    Ok(())
}
