use crate::payments::attempt::PaymentAttempt;
use crate::state::{AppState, notify};
use crate::{
    rpc::ChainCheckpoint,
    state::{WalletSyncStatus, now_unix, wallet_matches_tip},
    wallet::WALLET_BIRTHDAY_HEIGHT,
};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;
impl AppState {
    pub(crate) async fn synchronize_wallet(
        &self,
        target_height: Option<u64>,
    ) -> anyhow::Result<()> {
        self.synchronize_wallet_until(target_height, Instant::now() + Duration::from_secs(120))
            .await
    }

    pub(crate) async fn synchronize_wallet_until(
        &self,
        target_height: Option<u64>,
        deadline: Instant,
    ) -> anyhow::Result<()> {
        let _guard = tokio::time::timeout_at(deadline, self.0.wallet_sync.lock()).await?;

        {
            let mut snapshot = self.0.wallet_snapshot.write().await;
            snapshot.status.state = "syncing";
            snapshot.status.error = None;
        }
        notify(self, "sync");

        let result = async {
            if let Some(height) = target_height {
                let hash = crate::reconcile::within_deadline(
                    deadline,
                    "reading the mined block hash",
                    self.0.rpc.block_hash(u32::try_from(height)?),
                )
                .await?;
                self.0
                    .wallet
                    .wait_for_block(
                        &ChainCheckpoint { height, hash },
                        deadline.saturating_duration_since(Instant::now()),
                        |height| self.0.rpc.canonical_hash(height),
                    )
                    .await?;
            }
            let target = crate::reconcile::within_deadline(
                deadline,
                "reading the reconciliation target",
                self.0.rpc.checkpoint(),
            )
            .await?;
            if let Some(height) = target_height {
                anyhow::ensure!(
                    target.height >= height,
                    "mined height is no longer canonical"
                );
            }
            crate::reconcile::sync_wallet(&self.0.wallet, &self.0.rpc, &target, deadline).await?;
            self.refresh_wallet_snapshot_until(&target, Some(deadline))
                .await
        }
        .await;

        if let Err(error) = &result {
            let mut snapshot = self.0.wallet_snapshot.write().await;
            snapshot.status.state = "error";
            snapshot.status.error = Some(error.to_string());
            notify(self, "sync");
        }
        result
    }

    pub(crate) async fn synchronize_latest(&self) -> anyhow::Result<()> {
        self.synchronize_wallet(None).await
    }

    #[cfg(test)]
    pub(crate) async fn refresh_wallet_snapshot(
        &self,
        target: &ChainCheckpoint,
    ) -> anyhow::Result<()> {
        self.refresh_wallet_snapshot_until(target, None).await
    }

    pub(crate) async fn sync_if_chain_advanced(&self) -> anyhow::Result<()> {
        let (observed, lightwalletd) =
            tokio::join!(self.0.rpc.checkpoint(), self.0.wallet.latest_checkpoint());
        self.record_lightwalletd_reachability(lightwalletd.is_ok());
        let observed = observed?;
        let status = self.0.wallet_snapshot.read().await.status.clone();
        let settled = wallet_matches_tip(&status, &observed);
        {
            let mut snapshot = self.0.wallet_snapshot.write().await;
            snapshot.status.observed_height = Some(observed.height);
            snapshot.status.observed_hash = Some(observed.hash.clone());
        }
        if !settled {
            self.synchronize_wallet(Some(observed.height)).await?;
            notify(self, "chain");
        }
        Ok(())
    }

    /// The status handler probes lightwalletd itself; this only tells subscribers to ask again
    /// when the probe's answer changed while the chain stayed idle.
    fn record_lightwalletd_reachability(&self, reachable: bool) {
        let previous = self
            .0
            .lightwalletd_reachable
            .swap(reachable, Ordering::AcqRel);
        if previous != reachable {
            notify(self, "sync");
        }
    }
}

/// a scan that reached its target stays publishable after other requests mine past it, as long as
/// the node still has the scanned block.
async fn scan_is_publishable<F, Fut>(
    scanned: Option<&ChainCheckpoint>,
    target: &ChainCheckpoint,
    observed: &ChainCheckpoint,
    canonical_hash: F,
) -> anyhow::Result<bool>
where
    F: FnOnce(u32) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<String>>,
{
    Ok(match scanned {
        Some(scanned) if scanned == observed => scanned.height >= target.height,
        Some(scanned) if (target.height..observed.height).contains(&scanned.height) => {
            canonical_hash(u32::try_from(scanned.height)?).await? == scanned.hash
        }
        Some(_) => false,
        None => observed.height < u64::from(WALLET_BIRTHDAY_HEIGHT),
    })
}
pub(crate) async fn wallet_sync_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if let Err(error) = state.sync_if_chain_advanced().await {
            tracing::warn!(
                error = %format_args!("{error:#}"),
                "background wallet synchronization failed"
            );
            let mut snapshot = state.0.wallet_snapshot.write().await;
            snapshot.status.state = "error";
            snapshot.status.error = Some(error.to_string());
            drop(snapshot);
            notify(&state, "sync");
        }
    }
}
pub(crate) async fn dependencies_ready(state: &AppState) -> anyhow::Result<()> {
    state.0.rpc.chain_info().await?;
    state.synchronize_latest().await?;
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/sync/tests.rs"]
pub(crate) mod tests;

impl AppState {
    pub(crate) async fn refresh_wallet_snapshot_until(
        &self,
        target: &ChainCheckpoint,
        deadline: Option<Instant>,
    ) -> anyhow::Result<()> {
        let attempt = PaymentAttempt {
            state: self,
            dispatch_deadline: deadline,
        };
        let mut accounts = self.0.store.accounts()?;
        self.0.wallet.apply_balances(&mut accounts).await?;
        let scanned = self.0.wallet.scanned_checkpoint().await?;
        let rpc = &self.0.rpc;
        let observed = attempt
            .network(
                Duration::from_secs(30),
                "reading wallet snapshot checkpoint",
                rpc.checkpoint(),
            )
            .await?;
        let publishable = scan_is_publishable(scanned.as_ref(), target, &observed, |height| {
            attempt.network(
                Duration::from_secs(30),
                "validating wallet snapshot checkpoint",
                rpc.block_hash(height),
            )
        })
        .await?;
        if !publishable {
            return Err(crate::bootstrap::ReadinessError::CheckpointChanged.into());
        }
        let changed = self.0.wallet_snapshot.read().await.accounts != accounts;
        {
            let mut snapshot = self.0.wallet_snapshot.write().await;
            snapshot.accounts = accounts;
            snapshot.status = WalletSyncStatus {
                state: "ready",
                fully_scanned_height: scanned.as_ref().map(|checkpoint| checkpoint.height),
                fully_scanned_hash: scanned.as_ref().map(|checkpoint| checkpoint.hash.clone()),
                observed_height: Some(observed.height),
                observed_hash: Some(observed.hash),
                last_success_at: Some(now_unix()),
                error: None,
            };
        }
        notify(self, "sync");
        if changed {
            notify(self, "wallet");
        }
        Ok(())
    }
}
