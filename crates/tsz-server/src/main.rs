mod api;
mod db;
mod rpc;
mod wallet;

use std::{
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
        } => init(data_dir, config_dir),
        Command::Serve { data_dir } => serve(data_dir).await,
    }
}

fn init(data_dir: PathBuf, config_dir: PathBuf) -> Result<()> {
    fs::create_dir_all(&data_dir)?;
    fs::create_dir_all(&config_dir)?;
    let store = Store::open(data_dir.join("tsz.db"))?;
    store.initialize()?;
    wallet::RealWallet::open(&data_dir, &store.seed()?)?;
    let miner = store.account(TREASURY_ACCOUNT_ID)?.transparent_address;
    fs::write(config_dir.join("zakurad.toml"), zakura_config(&miner))?;
    println!("initialized five development accounts and a hidden treasury; miner address {miner}");
    Ok(())
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
    let wallet = wallet::RealWallet::open(&data_dir, &store.seed()?)?;
    let state = api::AppState::new(
        store,
        wallet,
        std::env::var("TSZ_ZAKURA_RPC").unwrap_or_else(|_| "http://127.0.0.1:18232".into()),
        std::env::var("TSZ_INSTANCE").unwrap_or_else(|_| "default".into()),
    );
    let deadline = Instant::now() + Duration::from_secs(120);
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
}
