#[cfg(test)]
use crate::mining::MiningRuntime;
use crate::{
    db::{Account, Store},
    mining::MiningCoordinator,
    rpc::{ChainCheckpoint, NodeRpc},
    wallet::RealWallet,
    wallet::WALLET_BIRTHDAY_HEIGHT,
};
use serde::Serialize;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, RwLock, broadcast};
#[derive(Clone)]
pub(crate) struct AppState(pub(crate) Arc<Inner>);
pub(crate) struct Inner {
    started: AtomicBool,
    pub(crate) mining: Arc<MiningCoordinator>,
    #[cfg(test)]
    pub(crate) mining_runtime: Option<Arc<dyn MiningRuntime>>,
    #[cfg(test)]
    pub(crate) chain_runtime: Option<Arc<dyn crate::chain::ChainRuntime>>,
    pub(crate) store: Store,
    pub(crate) wallet: RealWallet,
    pub(crate) rpc: NodeRpc,
    pub(crate) instance: String,
    pub(crate) events: broadcast::Sender<String>,
    pub(crate) payments: Mutex<()>,
    pub(crate) wallet_sync: Mutex<()>,
    pub(crate) treasury_replenishment: Mutex<()>,
    pub(crate) wallet_snapshot: RwLock<WalletSnapshot>,
}

#[derive(Clone)]
pub(crate) struct WalletSnapshot {
    pub(crate) accounts: Vec<Account>,
    pub(crate) status: WalletSyncStatus,
}

#[derive(Clone, Serialize)]
pub(crate) struct WalletSyncStatus {
    pub(crate) state: &'static str,
    pub(crate) fully_scanned_height: Option<u64>,
    pub(crate) fully_scanned_hash: Option<String>,
    pub(crate) observed_height: Option<u64>,
    pub(crate) observed_hash: Option<String>,
    pub(crate) last_success_at: Option<u64>,
    pub(crate) error: Option<String>,
}

impl AppState {
    pub(crate) fn started(&self) -> bool {
        self.0.started.load(Ordering::Acquire)
    }

    pub(crate) fn mark_started(&self) {
        self.0.started.store(true, Ordering::Release);
    }

    pub(crate) fn new(store: Store, wallet: RealWallet, rpc: String, instance: String) -> Self {
        let (events, _) = broadcast::channel(128);
        let accounts = store.accounts().unwrap_or_default();
        Self(Arc::new(Inner {
            started: AtomicBool::new(false),
            mining: Arc::new(MiningCoordinator::new()),
            #[cfg(test)]
            mining_runtime: None,
            #[cfg(test)]
            chain_runtime: None,
            store,
            wallet,
            rpc: NodeRpc::new(rpc),
            instance,
            events,
            payments: Mutex::new(()),
            wallet_sync: Mutex::new(()),
            treasury_replenishment: Mutex::new(()),
            wallet_snapshot: RwLock::new(WalletSnapshot {
                accounts,
                status: WalletSyncStatus {
                    state: "syncing",
                    fully_scanned_height: None,
                    fully_scanned_hash: None,
                    observed_height: None,
                    observed_hash: None,
                    last_success_at: None,
                    error: None,
                },
            }),
        }))
    }

    pub(crate) async fn accounts(&self) -> Vec<Account> {
        self.0.wallet_snapshot.read().await.accounts.clone()
    }

    pub(crate) async fn wallet_sync_status(&self) -> WalletSyncStatus {
        self.0.wallet_snapshot.read().await.status.clone()
    }
}
pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn notify(state: &AppState, topic: &str) {
    let _ = state.0.events.send(topic.to_owned());
}

#[cfg(test)]
#[path = "../tests/unit/state/tests.rs"]
pub(crate) mod tests;

pub(crate) fn wallet_matches_tip(wallet: &WalletSyncStatus, tip: &ChainCheckpoint) -> bool {
    wallet.state == "ready"
        && ((wallet.fully_scanned_height == Some(tip.height)
            && wallet.fully_scanned_hash.as_deref() == Some(&tip.hash))
            || (tip.height < u64::from(WALLET_BIRTHDAY_HEIGHT)
                && wallet.fully_scanned_height.is_none()
                && wallet.observed_height == Some(tip.height)
                && wallet.observed_hash.as_deref() == Some(&tip.hash)))
}
