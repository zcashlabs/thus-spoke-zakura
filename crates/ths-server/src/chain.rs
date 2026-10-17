use crate::{
    db::Store,
    rpc::{ChainCheckpoint, ChainInfo},
    state::{AppState, notify},
    wallet::WALLET_BIRTHDAY_HEIGHT,
};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

// The non-finalized window in the pinned Zakura 1.6.0 image.
pub(crate) const MAX_REORG_DEPTH: u32 = 1000;
const REGTEST_GENESIS_HASH: &str =
    "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327";

#[derive(Debug, thiserror::Error)]
pub(crate) enum ChainChangeError {
    #[error("depth must be between 1 and 1000 and retain at least block 1")]
    InvalidDepth,
    #[error("mining, payment execution, or another chain operation is active")]
    Busy,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChainChangeResult {
    pub(crate) depth: u32,
    pub(crate) old_tip: ChainCheckpoint,
    pub(crate) new_tip: ChainCheckpoint,
    pub(crate) removed_hashes: Vec<String>,
    pub(crate) replacement_hashes: Vec<String>,
    pub(crate) retained_txids: Vec<String>,
    pub(crate) failed_transactions: Vec<TransactionFailure>,
}

#[derive(Debug, Serialize)]
pub(crate) struct TransactionFailure {
    pub(crate) txid: String,
    pub(crate) error: String,
}

#[async_trait::async_trait]
pub(crate) trait ChainRuntime: Send + Sync {
    async fn chain_info(&self) -> Result<ChainInfo>;
    async fn block_hash(&self, height: u32) -> Result<String>;
    async fn block(&self, hash: &str) -> Result<Value>;
    async fn raw_transaction(&self, txid: &str) -> Result<Vec<u8>>;
    async fn invalidate(&self, hash: &str) -> Result<()>;
    async fn broadcast(&self, raw: &[u8]) -> Result<()>;
    async fn mempool(&self) -> Result<Vec<String>>;
    async fn generate_one(&self) -> Result<String>;
    async fn synchronize(&self) -> Result<()>;
}

struct ChainAdapter(AppState);

#[async_trait::async_trait]
impl ChainRuntime for ChainAdapter {
    async fn chain_info(&self) -> Result<ChainInfo> {
        self.0.0.rpc.chain_info().await
    }
    async fn block_hash(&self, height: u32) -> Result<String> {
        self.0.0.rpc.block_hash(height).await
    }
    async fn block(&self, hash: &str) -> Result<Value> {
        self.0.0.rpc.block(hash).await
    }
    async fn raw_transaction(&self, txid: &str) -> Result<Vec<u8>> {
        let tx = self
            .0
            .0
            .rpc
            .lookup_transaction(txid)
            .await?
            .context("dropped transaction is unavailable")?;
        hex::decode(
            tx["hex"]
                .as_str()
                .context("transaction omitted its bytes")?,
        )
        .context("invalid transaction hex")
    }
    async fn invalidate(&self, hash: &str) -> Result<()> {
        self.0.0.rpc.invalidate_block(hash).await
    }
    async fn broadcast(&self, raw: &[u8]) -> Result<()> {
        self.0.0.wallet.broadcast(raw).await
    }
    async fn mempool(&self) -> Result<Vec<String>> {
        self.0.0.rpc.mempool().await
    }
    async fn generate_one(&self) -> Result<String> {
        let mut hashes = self.0.0.rpc.generate(1).await?;
        anyhow::ensure!(
            hashes.len() == 1 && !hashes[0].is_empty(),
            "Zakura must return one nonempty replacement hash"
        );
        Ok(hashes.remove(0))
    }
    async fn synchronize(&self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            crate::reconcile::check_deadline(deadline)?;
            let attempt = async {
                self.0.synchronize_wallet_until(None, deadline).await?;
                let (node, indexed) = tokio::join!(
                    crate::reconcile::within_deadline(
                        deadline,
                        "checking canonical tip",
                        self.0.0.rpc.checkpoint()
                    ),
                    crate::reconcile::within_deadline(
                        deadline,
                        "checking indexed tip",
                        self.0.0.wallet.latest_checkpoint()
                    ),
                );
                let node = node?;
                if node != indexed?
                    || !crate::state::wallet_matches_tip(&self.0.wallet_sync_status().await, &node)
                {
                    return Err(crate::bootstrap::ReadinessError::CheckpointChanged.into());
                }
                Ok(())
            }
            .await;
            match attempt {
                Ok(()) => return Ok(()),
                Err(error) if crate::bootstrap::startup_retryable(&error) => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

pub(crate) fn validate_depth(depth: u32, height: u64) -> Result<u32> {
    let ancestor = height
        .checked_sub(u64::from(depth))
        .filter(|height| *height >= u64::from(WALLET_BIRTHDAY_HEIGHT - 1));
    if !(1..=MAX_REORG_DEPTH).contains(&depth) || ancestor.is_none() {
        return Err(ChainChangeError::InvalidDepth.into());
    }
    Ok(u32::try_from(ancestor.unwrap())?)
}

fn transaction_ids(block: &Value) -> Result<Vec<String>> {
    block["tx"]
        .as_array()
        .context("block omitted its transactions")?
        .iter()
        .map(|tx| {
            tx.as_str()
                .or_else(|| tx["txid"].as_str())
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .context("block transaction omitted its id")
        })
        .collect()
}

async fn wait_for_mempool(runtime: &dyn ChainRuntime, txids: &[String]) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let pool = runtime.mempool().await?;
            if txids.iter().all(|txid| pool.contains(txid)) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("transaction admission timed out")?
}

async fn change_chain_with(
    runtime: &dyn ChainRuntime,
    store: &Store,
    depth: u32,
    replace: bool,
    keep_txs: bool,
) -> Result<ChainChangeResult> {
    let info = runtime.chain_info().await?;
    // Zakura's BIP70 label is "test" for both Testnet and Regtest, so the
    // label alone cannot authorize changing the chain.
    anyhow::ensure!(
        matches!(info.chain.as_str(), "test" | "regtest")
            && runtime.block_hash(0).await? == REGTEST_GENESIS_HASH,
        "chain changes are Regtest-only"
    );
    anyhow::ensure!(
        info.bestblockhash.len() == 64,
        "Zakura returned an invalid best block hash"
    );
    let ancestor = validate_depth(depth, info.blocks)?;
    let old_tip = ChainCheckpoint {
        height: info.blocks,
        hash: info.bestblockhash.clone(),
    };
    let ancestor_hash = runtime.block_hash(ancestor).await?;
    let mut removed_hashes = Vec::with_capacity(depth as usize);
    let mut transactions = Vec::new();
    for height in ancestor + 1..=u32::try_from(old_tip.height)? {
        let hash = runtime.block_hash(height).await?;
        if keep_txs {
            let ids = transaction_ids(&runtime.block(&hash).await?)?;
            anyhow::ensure!(!ids.is_empty(), "block omitted its coinbase transaction");
            for txid in ids.into_iter().skip(1) {
                let raw = runtime.raw_transaction(&txid).await?;
                transactions.push((txid, raw));
            }
        }
        removed_hashes.push(hash);
    }
    let current = runtime.chain_info().await?;
    anyhow::ensure!(
        current.blocks == old_tip.height && current.bestblockhash == old_tip.hash,
        "chain changed while preparing the rollback"
    );

    // Persist loss of confirmation before changing the node. After interruption,
    // observation can reconfirm canonical transactions, but cannot auto-repay them.
    let confirmations = store.unconfirm_blocks(&removed_hashes)?;
    let mutation = async {
        runtime.invalidate(&removed_hashes[0]).await?;
        let tip = runtime.chain_info().await?;
        anyhow::ensure!(
            tip.blocks == u64::from(ancestor) && tip.bestblockhash == ancestor_hash,
            "invalidation did not reach the expected ancestor"
        );
        let mut replacement_hashes = Vec::new();
        let mut included = HashSet::new();
        let mut failures = HashMap::new();
        if replace {
            for offset in 1..=depth {
                let mut submitted = Vec::new();
                for (txid, raw) in &transactions {
                    if included.contains(txid) {
                        continue;
                    }
                    match runtime.broadcast(raw).await {
                        Ok(()) => submitted.push(txid.clone()),
                        Err(error) => {
                            failures.insert(txid.clone(), format!("{error:#}"));
                        }
                    }
                }
                if !submitted.is_empty()
                    && let Err(error) = wait_for_mempool(runtime, &submitted).await
                {
                    for txid in submitted {
                        failures.insert(txid, format!("{error:#}"));
                    }
                }
                let hash = runtime.generate_one().await?;
                anyhow::ensure!(
                    hash != removed_hashes[(offset - 1) as usize],
                    "replacement block retained its original hash"
                );
                anyhow::ensure!(
                    runtime.block_hash(ancestor + offset).await? == hash,
                    "replacement block is not canonical"
                );
                if keep_txs {
                    for txid in transaction_ids(&runtime.block(&hash).await?)? {
                        store.confirm_reorged_transaction(&txid, &hash)?;
                        included.insert(txid);
                    }
                }
                replacement_hashes.push(hash);
            }
        }
        let current = runtime.chain_info().await?;
        let expected_height = if replace {
            old_tip.height
        } else {
            u64::from(ancestor)
        };
        let expected_hash = replacement_hashes.last().unwrap_or(&ancestor_hash);
        anyhow::ensure!(
            current.blocks == expected_height && &current.bestblockhash == expected_hash,
            "chain changed during the operation"
        );
        let mut retained_txids = Vec::new();
        let mut failed_transactions = Vec::new();
        for (txid, _) in transactions {
            if included.contains(&txid) {
                retained_txids.push(txid);
            } else {
                let error = failures
                    .remove(&txid)
                    .unwrap_or_else(|| "transaction was not included in replacement blocks".into());
                failed_transactions.push(TransactionFailure { txid, error });
            }
        }
        Ok(ChainChangeResult {
            depth,
            old_tip,
            new_tip: ChainCheckpoint {
                height: current.blocks,
                hash: current.bestblockhash,
            },
            removed_hashes,
            replacement_hashes,
            retained_txids,
            failed_transactions,
        })
    }
    .await;
    // Even a failed mutation may have changed the chain; reconcile what actually
    // exists instead of automatically retrying invalidation or mining.
    let synchronized = async {
        let synchronized = runtime.synchronize().await;
        // A rejection (including a finalized block) must not leave still-canonical
        // payments orphaned. An uncertain response is safe to restore only after
        // proving that the original tip is still canonical.
        if mutation.is_err() {
            let current = runtime.chain_info().await?;
            if current.blocks == info.blocks && current.bestblockhash == info.bestblockhash {
                for (txid, hash) in confirmations {
                    store.confirm_reorged_transaction(&txid, &hash)?;
                }
            }
        }
        synchronized
    }
    .await;
    match (mutation, synchronized) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Ok(())) => Err(error.context("chain operation failed; inspect the current tip before retrying")),
        (Ok(_), Err(error)) => Err(error.context("chain changed, but wallet synchronization failed")),
        (Err(error), Err(sync_error)) => Err(error.context(format!("chain operation and synchronization failed: {sync_error:#}; inspect the current tip before retrying"))),
    }
}

impl AppState {
    pub(crate) async fn change_chain(
        &self,
        depth: u32,
        replace: bool,
        keep_txs: bool,
    ) -> Result<ChainChangeResult> {
        let _payment = self
            .0
            .payments
            .try_lock()
            .map_err(|_| ChainChangeError::Busy)?;
        let _chain = self
            .0
            .mining
            .chain_operations
            .clone()
            .try_write_owned()
            .map_err(|_| ChainChangeError::Busy)?;
        let runtime: Arc<dyn ChainRuntime> = Arc::new(ChainAdapter(self.clone()));
        #[cfg(test)]
        let runtime = self.0.chain_runtime.clone().unwrap_or(runtime);
        let result =
            change_chain_with(runtime.as_ref(), &self.0.store, depth, replace, keep_txs).await;
        notify(self, "chain");
        notify(self, "wallet");
        result
    }
}

#[cfg(test)]
#[path = "../tests/unit/chain/tests.rs"]
mod tests;
