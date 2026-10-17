use std::fmt::Write;

use anyhow::Result;

use crate::db::Store;

const DEVELOPMENT_CREDENTIAL_WARNING: &str = concat!(
    "WARNING: Never use this mnemonic or private key in production. ",
    "It is intended purely for development. Using it in production will result in loss of funds."
);

pub(super) fn development_credentials(store: &Store) -> Result<String> {
    let secrets = store.development_secrets()?;
    let mut output =
        String::from("\n⚠ DISPOSABLE REGTEST SECRETS — NEVER SEND REAL FUNDS TO THESE KEYS\n");
    writeln!(output, "{DEVELOPMENT_CREDENTIAL_WARNING}")?;
    writeln!(output, "Mnemonic: {}", secrets.mnemonic)?;
    for account in secrets.accounts {
        writeln!(
            output,
            "Account {}: {}",
            account.id, account.unified_address
        )?;
        writeln!(
            output,
            "  Unified spending key (hex): {}",
            account.unified_spending_key_hex
        )?;
    }
    Ok(output.trim_end().to_owned())
}

/// A zero-value payment to the P2SH address of an all-zero script hash. Every
/// chain that activates NU6.1 must carry the one-time ZIP-271 lockbox
/// disbursement in its activation-block coinbase, and Regtest configures none,
/// so Zakura rejects that block. This marker satisfies the rule without
/// creating funds or drawing on the empty local lockbox; Zakura's own local
/// network generator uses the same marker.
const LOCKBOX_MARKER_ADDRESS: &str = "t26YoyZ1iPgiMEWL4zGUm74eVWfhyDMXzY2";

pub(super) fn zakura_config(miner: &str) -> String {
    // Keep these heights in sync with `crate::network::regtest_network()`.
    format!(
        r#"[network]
network = "Regtest"
listen_addr = "0.0.0.0:18233"
[network.testnet_parameters]
lockbox_disbursements = [{{ address = "{LOCKBOX_MARKER_ADDRESS}", amount = 0 }}]
[network.testnet_parameters.activation_heights]
"NU7" = 1

[rpc]
listen_addr = "0.0.0.0:18232"
enable_cookie_auth = false

[state]
cache_dir = "/data"

[mining]
miner_address = "{miner}"
extra_coinbase_data = "thus-spoke-zakura"
"#
    )
}

#[cfg(test)]
#[path = "../../tests/unit/bootstrap/config/tests.rs"]
mod tests;
