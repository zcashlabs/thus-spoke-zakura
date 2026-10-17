use super::progression::PAYMENT_LOOKUP_TIMEOUT;
use crate::state::AppState;
use crate::state::notify;
use anyhow::Context;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

pub(crate) struct PaymentAttempt<'a> {
    pub(crate) state: &'a AppState,
    pub(crate) dispatch_deadline: Option<Instant>,
}

impl PaymentAttempt<'_> {
    pub(crate) fn check_can_start(&self) -> anyhow::Result<()> {
        if let Some(deadline) = self.dispatch_deadline {
            crate::reconcile::check_deadline(deadline)?;
        }
        Ok(())
    }

    pub(crate) fn network_deadline(&self, stage_limit: Duration) -> Instant {
        let stage_deadline = Instant::now() + stage_limit;
        self.dispatch_deadline
            .map_or(stage_deadline, |deadline| deadline.min(stage_deadline))
    }

    pub(crate) async fn network<T, E>(
        &self,
        stage_limit: Duration,
        label: &str,
        future: impl std::future::Future<Output = Result<T, E>>,
    ) -> anyhow::Result<T>
    where
        E: Into<anyhow::Error>,
    {
        self.check_can_start()?;
        crate::reconcile::within_deadline(self.network_deadline(stage_limit), label, future).await
    }

    pub(crate) async fn lock<'a>(
        &self,
        mutex: &'a Mutex<()>,
    ) -> anyhow::Result<tokio::sync::MutexGuard<'a, ()>> {
        self.check_can_start()?;
        match self.dispatch_deadline {
            Some(deadline) => Ok(tokio::time::timeout_at(deadline, mutex.lock()).await?),
            None => Ok(mutex.lock().await),
        }
    }

    pub(crate) async fn lookup_transaction(&self, txid: &str) -> anyhow::Result<Option<Value>> {
        let limit = if self.dispatch_deadline.is_some() {
            PAYMENT_LOOKUP_TIMEOUT
        } else {
            Duration::from_secs(30)
        };
        self.network(
            limit,
            "looking up payment transaction",
            self.state.0.rpc.lookup_transaction(txid),
        )
        .await
    }

    pub(crate) async fn synchronize_wallet(&self, height: Option<u64>) -> anyhow::Result<()> {
        self.check_can_start()?;
        self.state
            .synchronize_wallet_until(height, self.network_deadline(Duration::from_secs(120)))
            .await
    }

    pub(crate) async fn mine_and_sync(&self, blocks: u32) -> anyhow::Result<Vec<String>> {
        self.check_can_start()?;
        let _chain = self
            .network(
                Duration::from_secs(120),
                "waiting for chain operation",
                async { Ok::<_, anyhow::Error>(self.state.0.mining.chain_operations.read().await) },
            )
            .await?;
        let hashes = if self.dispatch_deadline.is_none() {
            self.state
                .0
                .rpc
                .generate(blocks)
                .await
                .context("automatically mining payment blocks")?
        } else {
            self.network(
                Duration::from_secs(3600),
                "automatically mining payment blocks",
                self.state.0.rpc.generate(blocks),
            )
            .await?
        };
        let tip_hash = hashes
            .last()
            .context("Zakura did not return the mined block hash")?;
        let block = self
            .network(
                Duration::from_secs(30),
                "reading automatically mined block",
                self.state.0.rpc.block(tip_hash),
            )
            .await?;
        let height = block
            .get("height")
            .and_then(Value::as_u64)
            .context("Zakura mined block omitted its height")?;
        self.synchronize_wallet(Some(height))
            .await
            .context("synchronizing automatically mined payment blocks")?;
        // Automatic mining moves the chain for every dashboard, including
        // clients that did not initiate this payment.
        notify(self.state, "chain");
        Ok(hashes)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/payments/attempt/tests.rs"]
pub(crate) mod tests;
