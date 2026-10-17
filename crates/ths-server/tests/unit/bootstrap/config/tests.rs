use super::*;
use crate::db::TREASURY_ACCOUNT_ID;

#[test]
fn configures_the_hidden_treasury_as_miner() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let user = store.account(1).unwrap().transparent_address;
    let treasury = store
        .account(TREASURY_ACCOUNT_ID)
        .unwrap()
        .transparent_address;
    let config = zakura_config(&treasury);
    assert!(config.contains(&format!("miner_address = \"{treasury}\"")));
    assert!(!config.contains(&format!("miner_address = \"{user}\"")));
}

#[test]
fn activates_ironwood_with_a_lockbox_marker() {
    let config = zakura_config("tm-miner");
    assert!(config.contains("[network.testnet_parameters.activation_heights]\n\"NU7\" = 1\n"));
    assert!(config.contains(&format!(
        "lockbox_disbursements = [{{ address = \"{LOCKBOX_MARKER_ADDRESS}\", amount = 0 }}]"
    )));
    let network = crate::network::regtest_network();
    assert!(matches!(
        zcash_keys::address::Address::decode(&network, LOCKBOX_MARKER_ADDRESS),
        Some(zcash_keys::address::Address::Transparent(_))
    ));
    assert_eq!(network.nu6_1, network.nu6);
    assert_eq!(network.nu6_2, network.nu6);
    assert_eq!(network.nu6_3, network.nu6);
    assert_eq!(network.nu7, network.nu6);
}

#[test]
fn selects_nu7_from_the_first_regtest_block() {
    use zcash_primitives::transaction::TxVersion;
    use zcash_protocol::consensus::BranchId;

    let network = crate::network::regtest_network();
    assert_eq!(BranchId::for_height(&network, 0.into()), BranchId::Sprout);
    for height in [1, 2] {
        let branch = BranchId::for_height(&network, height.into());
        assert_eq!(branch, BranchId::Nu7);
        assert_eq!(TxVersion::suggested_for_branch(branch), TxVersion::V6);
        assert!(!TxVersion::V4.valid_in_branch(branch));
    }
    assert!(zakura_config("tm-miner").contains("\"NU7\" = 1\n"));
}

#[test]
fn prints_mnemonic_and_exactly_five_user_account_keys() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();

    let output = development_credentials(&store).unwrap();
    let mnemonic = store.development_secrets().unwrap().mnemonic;
    let warning_at = output
        .find(DEVELOPMENT_CREDENTIAL_WARNING)
        .expect("startup credentials must warn against production use");
    let mnemonic_at = output
        .find("Mnemonic:")
        .expect("startup credentials must include the mnemonic");
    assert!(
        warning_at < mnemonic_at,
        "the production-use warning must appear next to the mnemonic and spending keys"
    );
    assert!(output.contains("Never use this mnemonic or private key in production"));
    assert!(output.contains("purely for development"));
    assert!(output.contains("will result in loss of funds"));
    assert!(output.contains(&format!("Mnemonic: {mnemonic}")));
    assert_eq!(
        hex::encode(mnemonic.parse::<bip39::Mnemonic>().unwrap().to_seed("")),
        store.seed().unwrap()
    );
    assert_eq!(output.matches("Unified spending key (hex):").count(), 5);
    for id in 1..=5 {
        assert!(output.contains(&format!("Account {id}:")));
    }
    assert!(!output.contains("Account 6:"));
}
