use crate::{reconcile, rpc};
use std::{fs, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};

use crate::{
    api,
    db::{Store, TREASURY_ACCOUNT_ID},
    payments::treasury,
    state::AppState,
    sync, wallet,
};
use config::{development_credentials, zakura_config};

mod config;

pub(super) fn init(data_dir: PathBuf, config_dir: PathBuf) -> Result<()> {
    fs::create_dir_all(&data_dir)?;
    fs::create_dir_all(&config_dir)?;
    let store = Store::open(data_dir.join("ths.db"))?;
    store.initialize()?;
    wallet::RealWallet::open(&data_dir, &store.seed()?)?;
    let miner = store.account(TREASURY_ACCOUNT_ID)?.transparent_address;
    fs::write(config_dir.join("zakurad.toml"), zakura_config(&miner))?;
    println!("initialized five development accounts and a hidden treasury; miner address {miner}");
    println!("{}", development_credentials(&store)?);
    Ok(())
}

pub(super) async fn serve(data_dir: PathBuf) -> Result<()> {
    fs::create_dir_all(&data_dir)?;
    let store = Store::open(data_dir.join("ths.db"))?;
    store.initialize()?;
    let wallet = wallet::RealWallet::open(&data_dir, &store.seed()?)?;
    let state = AppState::new(
        store,
        wallet,
        std::env::var("THS_ZAKURA_RPC").unwrap_or_else(|_| "http://127.0.0.1:18232".into()),
        std::env::var("THS_INSTANCE").unwrap_or_else(|_| "default".into()),
    );
    let address: SocketAddr = std::env::var("THS_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8080".into())
        .parse()
        .context("invalid THS_LISTEN")?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address, "serving stored status while startup continues");
    serve_with_background(
        listener,
        api::router(state.clone()),
        background_tasks(state),
    )
    .await
}

fn background_tasks(state: AppState) -> tokio::task::JoinSet<Result<()>> {
    let mut background = tokio::task::JoinSet::new();
    background.spawn(start_and_sync(state.clone()));
    background.spawn(async move {
        crate::payments::progression::payment_confirmation_loop(state).await;
        Ok(())
    });
    background
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReadinessError {
    #[error("chain is below the wallet birthday floor; waiting for canonical block 1")]
    BelowBirthday,
    #[error("lightwalletd has not indexed the canonical rewind checkpoint")]
    RewindCheckpoint,
    #[error("chain checkpoint changed before wallet publication")]
    CheckpointChanged,
    #[error("initial funding is awaiting confirmation")]
    FundingPending,
}

pub(crate) fn startup_retryable(error: &anyhow::Error) -> bool {
    if error.downcast_ref::<rpc::RpcUnavailable>().is_some()
        || error.downcast_ref::<rpc::RpcTransport>().is_some()
        || error.downcast_ref::<ReadinessError>().is_some()
        || error
            .downcast_ref::<reconcile::ReconciliationDeadline>()
            .is_some()
        || error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
    {
        return true;
    }
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        return error.is_connect() || error.is_timeout();
    }
    if let Some(error) = error.downcast_ref::<rpc::RpcError>() {
        return error.code == -28;
    }
    if let Some(error) = error.downcast_ref::<tonic::Status>() {
        // lightwalletd aborts block streams while repairing its cache after a
        // reorg. Restart readiness/synchronization, not the chain mutation.
        return matches!(
            error.code(),
            tonic::Code::Unavailable
                | tonic::Code::DeadlineExceeded
                | tonic::Code::ResourceExhausted
                | tonic::Code::Aborted
        );
    }
    error.downcast_ref::<tonic::transport::Error>().is_some()
}

pub(crate) async fn retry_startup<F, Fut>(mut attempt: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    loop {
        match attempt().await {
            Ok(()) => return Ok(()),
            Err(error) if startup_retryable(&error) => {
                tracing::info!(error = %format_args!("{error:#}"), "waiting for startup readiness");
                tokio::time::sleep(Duration::from_millis(750)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn start_and_sync(state: AppState) -> Result<()> {
    let already_provisioned = treasury::initial_balance_provisioned(&state)?;
    // Network stages have their own deadlines. Await provisioning to completion
    // so its payment guards remain owned while a wallet worker is running.
    retry_startup(|| async {
        sync::dependencies_ready(&state)
            .await
            .context("waiting for startup dependencies")?;
        treasury::provision_initial_balance_with_history(&state, already_provisioned)
            .await
            .context("provisioning Account 1 with 5 Ironwood ZEC")
    })
    .await?;
    state.mark_started();
    tracing::info!("dashboard ready");
    sync::wallet_sync_loop(state).await;
    Ok(())
}

async fn serve_with_background(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    mut background: tokio::task::JoinSet<Result<()>>,
) -> Result<()> {
    // A stopped or panicked recovery task must not leave HTTP serving as if
    // server-owned background work were still supervised.
    tokio::select! {
        result = axum::serve(listener, app).into_future() => result?,
        result = background.join_next() => {
            let task = result.context("background task set unexpectedly empty")?;
            task??;
            anyhow::bail!("server background task unexpectedly exited");
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/bootstrap/tests.rs"]
mod tests;
