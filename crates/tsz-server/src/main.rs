mod api;
mod db;
mod rpc;
mod wallet;

use std::{
    fmt::Write,
    fs,
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use db::{Store, TREASURY_ACCOUNT_ID};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "tsz-server", version)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Init {
        #[arg(long, default_value = "/data")]
        data_dir: PathBuf,
        #[arg(long, default_value = "/config")]
        config_dir: PathBuf,
        /// Initialize the wallet after an external node's real tree state is available.
        #[arg(long)]
        defer_wallet: bool,
    },
    Serve {
        #[arg(long, default_value = "/data")]
        data_dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tsz_server=info,tower_http=info".into()),
        )
        .init();
    match Args::parse().command {
        Command::Init {
            data_dir,
            config_dir,
            defer_wallet,
        } => init(data_dir, config_dir, defer_wallet),
        Command::Serve { data_dir } => serve(data_dir).await,
    }
}

fn init(data_dir: PathBuf, config_dir: PathBuf, defer_wallet: bool) -> Result<()> {
    fs::create_dir_all(&data_dir)?;
    fs::create_dir_all(&config_dir)?;
    let store = Store::open(data_dir.join("tsz.db"))?;
    store.initialize()?;
    if !defer_wallet {
        wallet::RealWallet::open(&data_dir, &store.seed()?)?;
    }
    let miner = store.account(TREASURY_ACCOUNT_ID)?.transparent_address;
    fs::write(config_dir.join("zakurad.toml"), zakura_config(&miner))?;
    println!("initialized five development accounts and a hidden treasury; miner address {miner}");
    println!("{}", development_credentials(&store)?);
    Ok(())
}

fn development_credentials(store: &Store) -> Result<String> {
    let secrets = store.development_secrets()?;
    let mut output =
        String::from("\n⚠ DISPOSABLE REGTEST SECRETS — NEVER SEND REAL FUNDS TO THESE KEYS\n");
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

fn zakura_config(miner: &str) -> String {
    format!(
        r#"[network]
network = "Regtest"
listen_addr = "0.0.0.0:18233"
[network.testnet_parameters.activation_heights]
"NU6" = 1

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

async fn serve(data_dir: PathBuf) -> Result<()> {
    fs::create_dir_all(&data_dir)?;
    let store = Store::open(data_dir.join("tsz.db"))?;
    store.initialize()?;
    let rpc_url =
        std::env::var("TSZ_ZAKURA_RPC").unwrap_or_else(|_| "http://127.0.0.1:18232".into());
    let external = std::env::var("TSZ_NODE_MODE").is_ok_and(|mode| mode == "external_rpc");
    let rpc = rpc::NodeRpc::new(rpc_url.clone());
    let info = rpc
        .validate_network()
        .await
        .context("validating the node's Regtest configuration")?;
    rpc.validate_treasury(&store.account(TREASURY_ACCOUNT_ID)?.transparent_address)
        .await?;
    let rpc = if external {
        validate_external_chain(&rpc, &data_dir).await?;
        // The wallet starts scanning at block 2. Bootstrap block 1 only after all
        // compatibility checks pass, and never replace an existing chain.
        if info.blocks == 0 {
            rpc.generate(1).await?;
        }
        // Record the chain before creating/scanning the wallet, so a failed
        // startup cannot later reuse a partial wallet against a different chain.
        let anchor = rpc.block("1").await?;
        let hash = anchor["hash"]
            .as_str()
            .context("external node omitted block 1's hash")?;
        let pending = data_dir.join("external-chain.json.tmp");
        fs::write(&pending, serde_json::to_vec(hash)?)?;
        fs::rename(pending, data_dir.join("external-chain.json"))?;
        rpc.with_chain_anchor(hash.to_owned())
    } else {
        rpc
    };
    let wallet = if external {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            match tokio::time::timeout(
                Duration::from_secs(10),
                wallet::RealWallet::open_external(&data_dir, &store.seed()?),
            )
            .await
            {
                Ok(Ok(wallet)) => break wallet,
                Ok(Err(error)) if Instant::now() < deadline => {
                    tracing::info!(%error, "waiting for the external node's block-1 tree state");
                    tokio::time::sleep(Duration::from_millis(750)).await;
                }
                Err(error) if Instant::now() < deadline => {
                    tracing::info!(%error, "waiting for the external node's block-1 tree state");
                    tokio::time::sleep(Duration::from_millis(750)).await;
                }
                Ok(Err(error)) => return Err(error).context("initializing the external wallet"),
                Err(error) => return Err(error).context("waiting for lightwalletd"),
            }
        }
    } else {
        wallet::RealWallet::open(&data_dir, &store.seed()?)?
    };
    let wallet = if external {
        wallet.with_chain_guard(rpc.clone())
    } else {
        wallet
    };
    let state = api::AppState::new(
        store,
        wallet,
        rpc,
        std::env::var("TSZ_INSTANCE").unwrap_or_else(|_| "default".into()),
    );
    let deadline = Instant::now() + Duration::from_secs(if external { 600 } else { 120 });
    loop {
        match api::dependencies_ready(&state).await {
            Ok(()) => break,
            Err(error) if Instant::now() < deadline => {
                tracing::info!(%error, "waiting for Zakura and lightwalletd");
                tokio::time::sleep(Duration::from_millis(750)).await;
            }
            Err(error) => return Err(error).context("waiting for startup dependencies"),
        }
    }
    api::provision_initial_balance(&state)
        .await
        .context("provisioning Account 1 with 5 Orchard ZEC")?;
    tokio::spawn(api::wallet_sync_loop(state.clone()));
    let app = api::router(state);
    let address: SocketAddr = std::env::var("TSZ_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8080".into())
        .parse()
        .context("invalid TSZ_LISTEN")?;
    tracing::info!(%address, "dashboard ready");
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn validate_external_chain(rpc: &rpc::NodeRpc, data_dir: &std::path::Path) -> Result<()> {
    let path = data_dir.join("external-chain.json");
    if path.exists() {
        let expected: String = serde_json::from_slice(&fs::read(path)?)?;
        let actual: serde_json::Value = rpc.block("1").await.context(
            "external chain lost the wallet's anchor; reset the ths wallet and prepare again",
        )?;
        anyhow::ensure!(
            actual["hash"].as_str() == Some(expected.as_str()),
            "external chain changed at the wallet's birthday; reset the ths wallet and prepare again"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn prints_mnemonic_and_exactly_five_user_account_keys() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();

        let output = development_credentials(&store).unwrap();
        let mnemonic = store.development_secrets().unwrap().mnemonic;
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
}
