use super::*;
use crate::db::TREASURY_ACCOUNT_ID;
use crate::network::DEFAULT_NU7_ACTIVATION_HEIGHT;

#[test]
fn configures_the_hidden_treasury_as_miner() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let user = store.account(1).unwrap().transparent_address;
    let treasury = store
        .account(TREASURY_ACCOUNT_ID)
        .unwrap()
        .transparent_address;
    let config = zakura_config(&treasury, DEFAULT_NU7_ACTIVATION_HEIGHT);
    assert!(config.contains(&format!("miner_address = \"{treasury}\"")));
    assert!(!config.contains(&format!("miner_address = \"{user}\"")));
}

#[test]
fn activates_ironwood_with_a_lockbox_marker() {
    let config = zakura_config("tm-miner", DEFAULT_NU7_ACTIVATION_HEIGHT);
    assert!(config.contains(
        "[network.testnet_parameters.activation_heights]\n\"NU5\" = 1\n\"NU6\" = 1\n\"NU6.1\" = 1\n\"NU6.2\" = 1\n\"NU6.3\" = 1\n\"NU7\" = 1\n"
    ));
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
    assert!(zakura_config("tm-miner", DEFAULT_NU7_ACTIVATION_HEIGHT).contains("\"NU7\" = 1\n"));
}

/// Zakura moves every unlisted upgrade after Canopy to the next listed height,
/// so the config must name each one with the height the server's wallet uses.
#[test]
fn zakura_config_names_every_upgrade_after_canopy_at_the_server_height() {
    for nu7 in [DEFAULT_NU7_ACTIVATION_HEIGHT, 150] {
        let config = zakura_config("tm-miner", nu7);
        let configured: std::collections::BTreeMap<&str, u32> = config
            .split("[network.testnet_parameters.activation_heights]\n")
            .nth(1)
            .unwrap()
            .lines()
            .take_while(|line| !line.is_empty())
            .map(|line| {
                let (name, height) = line.split_once(" = ").unwrap();
                (name.trim_matches('"'), height.parse().unwrap())
            })
            .collect();
        let network = crate::network::regtest_network_with_nu7(nu7);
        let height = |upgrade: Option<zcash_protocol::consensus::BlockHeight>| {
            u32::from(upgrade.expect("every Regtest upgrade has a height"))
        };
        let expected = std::collections::BTreeMap::from([
            ("NU5", height(network.nu5)),
            ("NU6", height(network.nu6)),
            ("NU6.1", height(network.nu6_1)),
            ("NU6.2", height(network.nu6_2)),
            ("NU6.3", height(network.nu6_3)),
            ("NU7", height(network.nu7)),
        ]);
        assert_eq!(configured, expected, "NU7 at {nu7}");
    }
}

#[test]
fn a_later_nu7_height_keeps_ironwood_from_height_one_and_switches_branch_at_activation() {
    use zcash_primitives::transaction::TxVersion;
    use zcash_protocol::consensus::BranchId;

    let network = crate::network::regtest_network_with_nu7(150);
    for height in [1, 2, 105, 149] {
        let branch = BranchId::for_height(&network, height.into());
        assert_eq!(branch, BranchId::Nu6_3, "height {height}");
        assert_eq!(TxVersion::suggested_for_branch(branch), TxVersion::V6);
    }
    for height in [150, 151] {
        assert_eq!(
            BranchId::for_height(&network, height.into()),
            BranchId::Nu7,
            "height {height}"
        );
    }
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

#[test]
fn native_config_keeps_all_network_listeners_local() {
    let native = crate::args::NativeInit {
        native_chain_dir: Some(std::path::PathBuf::from("/tmp/chain \"x\"")),
        native_rpc_port: Some(12345),
        native_p2p_port: Some(12346),
    };
    let config = configured_zakura("tm-miner", &native, 150).unwrap();
    assert!(config.contains("p2p_stack = \"legacy\""));
    assert!(config.contains("listen_addr = \"127.0.0.1:12345\""));
    assert!(config.contains("listen_addr = \"127.0.0.1:12346\""));
    assert!(!config.contains("0.0.0.0"));
    assert!(config.contains("cache_dir = \"/tmp/chain \\\"x\\\"\""));
    assert!(config.contains("network = \"Regtest\""));
    assert!(config.contains("\"NU7\" = 150\n"));
    assert!(config.contains("\"NU6.3\" = 1\n"));
}
