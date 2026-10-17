use super::attempt::PaymentAttempt;
use super::confirmation::confirmed_block_hash;
use super::progression::PAYMENT_LOOKUP_TIMEOUT;
use super::progression::execute_activity_intent;
use super::submission::OriginalTransactionState;
use super::submission::PaymentSubmitter;
use super::submission::original_presence;
use super::submission::original_presence_from_lookup;
use super::submission::original_transaction_state;
use crate::db::Account;
use crate::db::Activity;
use crate::db::Store;
use crate::db::TREASURY_ACCOUNT_ID;
use crate::db::ZATOSHIS_PER_ZEC;
use crate::network::regtest_network;
use crate::rpc::ChainCheckpoint;
use crate::state::AppState;
use crate::wallet::PaymentError;
use crate::wallet::PreparedPayment;
use crate::wallet::WALLET_BIRTHDAY_HEIGHT;
use anyhow::Context;
use serde_json::Value;
use std::time::Duration;
use zcash_keys::encoding::AddressCodec;
use zcash_protocol::consensus::COINBASE_MATURITY_BLOCKS;

pub(crate) fn record_prepared_address_payment(
    store: &Store,
    id: &str,
) -> impl FnOnce(PreparedPayment) -> anyhow::Result<PreparedPayment> + Clone + Send + 'static {
    let store = store.clone();
    let id = id.to_owned();
    move |prepared| {
        store.record_address_prepared(
            &id,
            &prepared.txid,
            &prepared.raw_transaction,
            prepared.expiry_height,
        )?;
        Ok(prepared)
    }
}

#[async_trait::async_trait]
pub(crate) trait FaucetRuntime: Sync {
    async fn prepare_payment<F>(
        &self,
        activity_id: Option<&str>,
        replacement_txid: Option<&str>,
        seed: &str,
        destination: &str,
        amount_zatoshi: u64,
        record: F,
    ) -> anyhow::Result<PreparedPayment>
    where
        F: FnOnce(PreparedPayment) -> anyhow::Result<PreparedPayment> + Send + 'static;
    async fn chain_height(&self) -> anyhow::Result<u64>;
    async fn block_hash(&self, height: u32) -> anyhow::Result<String>;
    async fn synchronize_latest(&self) -> anyhow::Result<()>;
    async fn treasury_cursor(&self) -> anyhow::Result<Option<crate::db::TreasuryCursor>>;
    async fn initialize_treasury_cursor(
        &self,
        receiver: &str,
        height: u32,
        hash: &str,
    ) -> anyhow::Result<crate::db::TreasuryCursor>;
    async fn discover_reward(
        &self,
        cursor: &crate::db::TreasuryCursor,
        treasury: &Account,
    ) -> anyhow::Result<crate::db::TreasuryCursor>;
    async fn shield_coinbase(
        &self,
        seed: &str,
        treasury: &Account,
        minimum_net: u64,
    ) -> anyhow::Result<()>;
    async fn resume_shielding(&self) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn shield_and_sync(
        &self,
        seed: &str,
        treasury: &Account,
        minimum_net: u64,
    ) -> anyhow::Result<()> {
        self.shield_coinbase(seed, treasury, minimum_net).await?;
        self.mine_and_sync(1).await
    }
    async fn mine_and_sync(&self, blocks: u32) -> anyhow::Result<()>;
    async fn mine_to_height(&self, height: u64) -> anyhow::Result<()> {
        let current = self.chain_height().await?;
        if height > current {
            self.mine_and_sync(u32::try_from(height - current)?).await?;
        }
        anyhow::ensure!(
            self.chain_height().await? >= height,
            "maturity mining did not reach height {height}"
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl FaucetRuntime for AppState {
    async fn prepare_payment<F>(
        &self,
        activity_id: Option<&str>,
        replacement_txid: Option<&str>,
        seed: &str,
        destination: &str,
        amount_zatoshi: u64,
        record: F,
    ) -> anyhow::Result<PreparedPayment>
    where
        F: FnOnce(PreparedPayment) -> anyhow::Result<PreparedPayment> + Send + 'static,
    {
        FaucetRuntime::prepare_payment(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            activity_id,
            replacement_txid,
            seed,
            destination,
            amount_zatoshi,
            record,
        )
        .await
    }
    async fn chain_height(&self) -> anyhow::Result<u64> {
        FaucetRuntime::chain_height(&PaymentAttempt {
            state: self,
            dispatch_deadline: None,
        })
        .await
    }
    async fn block_hash(&self, height: u32) -> anyhow::Result<String> {
        FaucetRuntime::block_hash(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            height,
        )
        .await
    }
    async fn synchronize_latest(&self) -> anyhow::Result<()> {
        FaucetRuntime::synchronize_latest(&PaymentAttempt {
            state: self,
            dispatch_deadline: None,
        })
        .await
    }
    async fn treasury_cursor(&self) -> anyhow::Result<Option<crate::db::TreasuryCursor>> {
        FaucetRuntime::treasury_cursor(&PaymentAttempt {
            state: self,
            dispatch_deadline: None,
        })
        .await
    }
    async fn initialize_treasury_cursor(
        &self,
        receiver: &str,
        height: u32,
        hash: &str,
    ) -> anyhow::Result<crate::db::TreasuryCursor> {
        FaucetRuntime::initialize_treasury_cursor(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            receiver,
            height,
            hash,
        )
        .await
    }
    async fn discover_reward(
        &self,
        cursor: &crate::db::TreasuryCursor,
        treasury: &Account,
    ) -> anyhow::Result<crate::db::TreasuryCursor> {
        FaucetRuntime::discover_reward(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            cursor,
            treasury,
        )
        .await
    }
    async fn shield_coinbase(
        &self,
        seed: &str,
        treasury: &Account,
        minimum_net: u64,
    ) -> anyhow::Result<()> {
        FaucetRuntime::shield_coinbase(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            seed,
            treasury,
            minimum_net,
        )
        .await
    }
    async fn resume_shielding(&self) -> anyhow::Result<bool> {
        FaucetRuntime::resume_shielding(&PaymentAttempt {
            state: self,
            dispatch_deadline: None,
        })
        .await
    }
    async fn shield_and_sync(
        &self,
        seed: &str,
        treasury: &Account,
        minimum_net: u64,
    ) -> anyhow::Result<()> {
        FaucetRuntime::shield_coinbase(self, seed, treasury, minimum_net).await
    }
    async fn mine_and_sync(&self, blocks: u32) -> anyhow::Result<()> {
        FaucetRuntime::mine_and_sync(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            blocks,
        )
        .await
    }
}

#[async_trait::async_trait]
impl FaucetRuntime for PaymentAttempt<'_> {
    async fn prepare_payment<F>(
        &self,
        activity_id: Option<&str>,
        replacement_txid: Option<&str>,
        seed: &str,
        destination: &str,
        amount_zatoshi: u64,
        record: F,
    ) -> anyhow::Result<PreparedPayment>
    where
        F: FnOnce(PreparedPayment) -> anyhow::Result<PreparedPayment> + Send + 'static,
    {
        self.check_can_start()?;
        self.state
            .0
            .wallet
            .prepare(
                activity_id,
                replacement_txid,
                seed,
                TREASURY_ACCOUNT_ID,
                "ironwood",
                destination,
                amount_zatoshi,
                None,
                record,
            )
            .await
    }

    async fn chain_height(&self) -> anyhow::Result<u64> {
        let limit = if self.dispatch_deadline.is_some() {
            PAYMENT_LOOKUP_TIMEOUT
        } else {
            Duration::from_secs(30)
        };
        Ok(self
            .network(
                limit,
                "reading treasury chain height",
                self.state.0.rpc.chain_info(),
            )
            .await?
            .blocks)
    }

    async fn block_hash(&self, height: u32) -> anyhow::Result<String> {
        self.network(
            Duration::from_secs(30),
            "reading treasury block hash",
            self.state.0.rpc.block_hash(height),
        )
        .await
    }
    async fn synchronize_latest(&self) -> anyhow::Result<()> {
        self.synchronize_wallet(None).await
    }
    async fn treasury_cursor(&self) -> anyhow::Result<Option<crate::db::TreasuryCursor>> {
        self.check_can_start()?;
        self.state.0.wallet.treasury_cursor().await
    }
    async fn initialize_treasury_cursor(
        &self,
        receiver: &str,
        height: u32,
        hash: &str,
    ) -> anyhow::Result<crate::db::TreasuryCursor> {
        self.check_can_start()?;
        self.state
            .0
            .wallet
            .initialize_treasury_cursor(receiver, height, hash)
            .await
    }
    async fn discover_reward(
        &self,
        cursor: &crate::db::TreasuryCursor,
        treasury: &Account,
    ) -> anyhow::Result<crate::db::TreasuryCursor> {
        let _sync = self.lock(&self.state.0.wallet_sync).await?;
        discover_reward_with_attempt(self, cursor, treasury).await
    }
    async fn resume_shielding(&self) -> anyhow::Result<bool> {
        self.check_can_start()?;
        let Some(prepared) = self.state.0.wallet.prepared_shielding().await? else {
            return Ok(false);
        };
        let original =
            original_transaction_state(self, &prepared.txid, prepared.expiry_height).await?;
        if original == OriginalTransactionState::Expired {
            return Ok(false);
        }
        self.complete_shielding(&prepared, original).await?;
        Ok(true)
    }

    async fn shield_coinbase(
        &self,
        seed: &str,
        treasury: &Account,
        minimum_net: u64,
    ) -> anyhow::Result<()> {
        self.check_can_start()?;
        let retained = self.state.0.wallet.prepared_shielding().await?;
        let replacement_txid = if let Some(prepared) = &retained {
            let original =
                original_transaction_state(self, &prepared.txid, prepared.expiry_height).await?;
            if original != OriginalTransactionState::Expired {
                return self.complete_shielding(prepared, original).await;
            }
            Some(prepared.txid.as_str())
        } else {
            None
        };
        self.check_can_start()?;
        let prepared = self
            .state
            .0
            .wallet
            .prepare_shield_coinbase(
                seed,
                TREASURY_ACCOUNT_ID,
                &treasury.transparent_address,
                &treasury.unified_address,
                minimum_net,
                replacement_txid,
            )
            .await?;
        let original =
            original_transaction_state(self, &prepared.txid, prepared.expiry_height).await?;
        self.complete_shielding(&prepared, original).await
    }

    async fn shield_and_sync(
        &self,
        seed: &str,
        treasury: &Account,
        minimum_net: u64,
    ) -> anyhow::Result<()> {
        FaucetRuntime::shield_coinbase(self, seed, treasury, minimum_net).await
    }

    async fn mine_and_sync(&self, blocks: u32) -> anyhow::Result<()> {
        PaymentAttempt::mine_and_sync(self, blocks).await?;
        Ok(())
    }
}

impl PaymentAttempt<'_> {
    pub(crate) async fn complete_shielding(
        &self,
        prepared: &PreparedPayment,
        original: OriginalTransactionState,
    ) -> anyhow::Result<()> {
        match original {
            OriginalTransactionState::Included(hash) => {
                return self.synchronize_shielding(prepared, &hash).await;
            }
            OriginalTransactionState::Expired => {
                anyhow::bail!(
                    "retained shielding {} expired; replenishment remains pending",
                    prepared.txid
                );
            }
            OriginalTransactionState::Present => {}
            OriginalTransactionState::Missing => {
                PaymentSubmitter::broadcast(self, &prepared.raw_transaction)
                    .await
                    .with_context(|| {
                        format!("shielding {} broadcast remains pending", prepared.txid)
                    })?;
            }
        }
        // A prior mine request may have completed after its response was lost.
        // Reconcile the retained identity at the selected tip before mining again.
        PaymentSubmitter::chain_height(self).await?;
        match original_transaction_state(self, &prepared.txid, prepared.expiry_height).await? {
            OriginalTransactionState::Included(hash) => {
                return self.synchronize_shielding(prepared, &hash).await;
            }
            OriginalTransactionState::Missing | OriginalTransactionState::Present => {}
            OriginalTransactionState::Expired => {
                anyhow::bail!(
                    "retained shielding {} expired; replenishment remains pending",
                    prepared.txid
                );
            }
        }
        self.mine_and_sync(1)
            .await
            .with_context(|| format!("shielding {} mining remains pending", prepared.txid))?;
        match original_presence(self, &prepared.txid).await? {
            OriginalTransactionState::Included(hash) => {
                self.synchronize_shielding(prepared, &hash).await
            }
            _ => anyhow::bail!("shielding {} inclusion remains pending", prepared.txid),
        }
    }

    pub(crate) async fn synchronize_shielding(
        &self,
        prepared: &PreparedPayment,
        hash: &str,
    ) -> anyhow::Result<()> {
        let height = PaymentSubmitter::inclusion_height(self, hash).await?;
        self.synchronize_wallet(Some(height)).await?;
        anyhow::ensure!(
            PaymentSubmitter::canonical_hash(self, u32::try_from(height)?).await? == hash,
            "shielding inclusion changed during synchronization; replenishment remains pending"
        );
        self.check_can_start()?;
        self.state.0.wallet.retire_shielding(&prepared.txid).await
    }
}

pub(crate) async fn discover_reward_with_attempt(
    attempt: &PaymentAttempt<'_>,
    cursor: &crate::db::TreasuryCursor,
    treasury: &Account,
) -> anyhow::Result<crate::db::TreasuryCursor> {
    let state = attempt.state;
    attempt.check_can_start()?;
    let next_height = cursor
        .height
        .checked_add(1)
        .context("treasury discovery height overflow")?;
    let block_hash = attempt
        .network(
            Duration::from_secs(30),
            "reading treasury discovery hash",
            state.0.rpc.block_hash(next_height),
        )
        .await?;
    let block = attempt
        .network(
            Duration::from_secs(30),
            "reading treasury discovery block",
            state.0.rpc.block(&block_hash),
        )
        .await?;
    anyhow::ensure!(
        block.get("height").and_then(Value::as_u64) == Some(u64::from(next_height)),
        "Zakura returned the wrong treasury discovery height"
    );
    let candidate = coinbase_candidate(&block, &treasury.transparent_address)?;
    let mut required_spenders = Vec::new();
    if let Some(candidate) = &candidate {
        for index in &candidate.output_indexes {
            if attempt
                .network(
                    Duration::from_secs(30),
                    "reading treasury output",
                    state.0.rpc.unspent_output(&candidate.txid, *index),
                )
                .await?
                .is_none()
            {
                attempt.check_can_start()?;
                let spenders = state
                    .0
                    .wallet
                    .known_spending_transactions(&candidate.txid, *index)
                    .await?;
                let mut accepted = Vec::new();
                for txid in spenders {
                    let Some(transaction) = attempt.lookup_transaction(&txid).await? else {
                        continue;
                    };
                    if let Some(hash) = confirmed_block_hash(&transaction) {
                        let height = attempt
                            .network(
                                Duration::from_secs(30),
                                "reading treasury spender block",
                                state.0.rpc.block(hash),
                            )
                            .await?
                            .get("height")
                            .and_then(Value::as_u64)
                            .context("spender block omitted its height")?;
                        anyhow::ensure!(
                            attempt
                                .network(
                                    Duration::from_secs(30),
                                    "validating treasury spender block",
                                    state.0.rpc.block_hash(u32::try_from(height)?)
                                )
                                .await?
                                == hash,
                            "spender block is not canonical"
                        );
                    } else if !attempt
                        .network(
                            Duration::from_secs(30),
                            "reading treasury spender mempool",
                            state.0.rpc.mempool(),
                        )
                        .await?
                        .contains(&txid)
                    {
                        continue;
                    }
                    accepted.push((*index, txid));
                }
                anyhow::ensure!(
                    !accepted.is_empty(),
                    "treasury output has no known spender; discovery remains pending"
                );
                required_spenders.extend(accepted);
            }
        }
    }
    let checkpoint = ChainCheckpoint {
        height: u64::from(next_height),
        hash: block_hash,
    };
    anyhow::ensure!(
        attempt
            .network(
                Duration::from_secs(30),
                "reading treasury discovery hash",
                state.0.rpc.block_hash(next_height)
            )
            .await?
            == checkpoint.hash,
        "canonical block changed during treasury discovery"
    );
    attempt.check_can_start()?;
    let next = state
        .0
        .wallet
        .commit_treasury_discovery(
            cursor,
            &checkpoint,
            candidate.as_ref().map(|value| value.raw.as_str()),
            &required_spenders,
        )
        .await?;
    anyhow::ensure!(
        attempt
            .network(
                Duration::from_secs(30),
                "reading treasury discovery hash",
                state.0.rpc.block_hash(next_height)
            )
            .await?
            == checkpoint.hash,
        "canonical block changed after treasury discovery"
    );
    Ok(next)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_with_replenishment<R, F>(
    runtime: &R,
    activity_id: Option<&str>,
    replacement_txid: Option<&str>,
    seed: &str,
    treasury: &Account,
    destination: &str,
    amount_zatoshi: u64,
    record: F,
) -> anyhow::Result<PreparedPayment>
where
    R: FaucetRuntime,
    F: FnOnce(PreparedPayment) -> anyhow::Result<PreparedPayment> + Clone + Send + 'static,
{
    runtime.resume_shielding().await?;
    let mut reconciled_after_scan_required = false;
    loop {
        match runtime
            .prepare_payment(
                activity_id,
                replacement_txid,
                seed,
                destination,
                amount_zatoshi,
                record.clone(),
            )
            .await
        {
            Ok(prepared) => return Ok(prepared),
            Err(error) => {
                if matches!(error.downcast_ref(), Some(PaymentError::ScanRequired)) {
                    if !reconciled_after_scan_required {
                        runtime.synchronize_latest().await?;
                        reconciled_after_scan_required = true;
                        continue;
                    }
                    if runtime.chain_height().await? < u64::from(WALLET_BIRTHDAY_HEIGHT) {
                        runtime
                            .mine_to_height(u64::from(WALLET_BIRTHDAY_HEIGHT))
                            .await?;
                        runtime.synchronize_latest().await?;
                        continue;
                    }
                    return Err(error);
                }
                let Some(PaymentError::InsufficientFunds {
                    available,
                    required,
                }) = error.downcast_ref()
                else {
                    return Err(error);
                };
                let minimum_net = required
                    .checked_sub(*available)
                    .filter(|value| *value > 0)
                    .context("SDK insufficient-funds result has no positive deficit")?;
                replenish_treasury(runtime, seed, treasury, minimum_net).await?;
                reconciled_after_scan_required = false;
            }
        }
    }
}

pub(crate) async fn replenish_treasury<R: FaucetRuntime>(
    runtime: &R,
    seed: &str,
    treasury: &Account,
    minimum_net: u64,
) -> anyhow::Result<()> {
    if runtime.resume_shielding().await? {
        return Ok(());
    }
    let boundary = WALLET_BIRTHDAY_HEIGHT - 1;
    runtime.mine_to_height(u64::from(boundary)).await?;
    let mut cursor = match runtime.treasury_cursor().await? {
        Some(cursor) => {
            anyhow::ensure!(
                cursor.receiver == treasury.transparent_address,
                "treasury cursor belongs to a different receiver"
            );
            cursor
        }
        None => {
            let hash = runtime.block_hash(boundary).await?;
            runtime
                .initialize_treasury_cursor(&treasury.transparent_address, boundary, &hash)
                .await?
        }
    };
    if runtime.chain_height().await? < u64::from(cursor.height)
        || runtime.block_hash(cursor.height).await? != cursor.block_hash
    {
        runtime.synchronize_latest().await?;
        cursor = runtime
            .treasury_cursor()
            .await?
            .context("treasury cursor disappeared during rewind")?;
        anyhow::ensure!(
            runtime.chain_height().await? >= u64::from(cursor.height)
                && runtime.block_hash(cursor.height).await? == cursor.block_hash,
            "treasury cursor is not canonical after wallet rewind"
        );
    }

    loop {
        match runtime.shield_and_sync(seed, treasury, minimum_net).await {
            Ok(()) => return Ok(()),
            Err(error) if matches!(error.downcast_ref(), Some(PaymentError::TreasuryExhausted)) => {
            }
            Err(error) => return Err(error),
        }
        let next_height = cursor
            .height
            .checked_add(1)
            .context("treasury discovery height overflow")?;
        // The SDK constructs a spend at tip + 1; 100 confirmations make this reward eligible there.
        let required_height = u64::from(next_height) + u64::from(COINBASE_MATURITY_BLOCKS) - 1;
        runtime
            .mine_to_height(required_height)
            .await
            .with_context(|| {
                format!("treasury maturity mining to height {required_height} remains pending")
            })?;
        cursor = runtime.discover_reward(&cursor, treasury).await?;
    }
}

pub(crate) struct CoinbaseCandidate {
    txid: String,
    raw: String,
    output_indexes: Vec<u32>,
}

pub(crate) fn coinbase_candidate(
    block: &Value,
    receiver: &str,
) -> anyhow::Result<Option<CoinbaseCandidate>> {
    let height = block
        .get("height")
        .and_then(Value::as_u64)
        .and_then(|height| u32::try_from(height).ok())
        .context("Zakura block omitted its height")?;
    let transaction = block
        .get("tx")
        .and_then(Value::as_array)
        .and_then(|transactions| transactions.first())
        .context("Zakura block omitted its coinbase transaction")?;
    let raw = transaction
        .get("hex")
        .and_then(Value::as_str)
        .context("Zakura coinbase omitted its raw transaction")?;
    let bytes = hex::decode(raw).context("invalid treasury transaction hex")?;
    let decoded = zcash_primitives::transaction::Transaction::read(
        &bytes[..],
        zcash_protocol::consensus::BranchId::for_height(&regtest_network(), height.into()),
    )?;
    let txid = decoded.txid().to_string();
    anyhow::ensure!(
        transaction.get("txid").and_then(Value::as_str) == Some(txid.as_str()),
        "Zakura coinbase transaction ID does not match its raw transaction"
    );
    let bundle = decoded
        .transparent_bundle()
        .filter(|bundle| bundle.is_coinbase())
        .context("Zakura block's first transaction is not coinbase")?;
    let output_indexes = bundle
        .vout
        .iter()
        .enumerate()
        .filter_map(|(index, output)| {
            output
                .recipient_address()
                .filter(|address| address.encode(&regtest_network()) == receiver)
                .map(|_| u32::try_from(index).context("treasury output index exceeds u32"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if output_indexes.is_empty() {
        return Ok(None);
    }
    Ok(Some(CoinbaseCandidate {
        txid,
        raw: raw.to_owned(),
        output_indexes,
    }))
}

pub(crate) const INITIAL_FUNDING_KEY: &str = "startup-account-1-ironwood-v1";

pub(crate) fn initial_balance_provisioned(state: &AppState) -> anyhow::Result<bool> {
    Ok(state
        .0
        .store
        .activity_for_key(INITIAL_FUNDING_KEY)?
        .is_some_and(|activity| activity.status == "confirmed"))
}

pub(crate) async fn provision_initial_balance_with_history(
    state: &AppState,
    already_provisioned: bool,
) -> anyhow::Result<()> {
    let existing = state.0.store.activity_for_key(INITIAL_FUNDING_KEY)?;
    // A deliberate rollback of completed provisioning must not fund Account 1
    // again or prevent the restarted environment from becoming ready.
    if existing
        .as_ref()
        .is_some_and(|activity| activity.status == "unconfirmed")
    {
        return Ok(());
    }
    state
        .0
        .store
        .claim_faucet(1, "ironwood", 5 * ZATOSHIS_PER_ZEC, INITIAL_FUNDING_KEY)?;
    if existing.is_none() {
        // A fresh wallet needs scanned blocks before a proposal can determine its target height.
        let seed = state.0.store.seed()?;
        let treasury = state.0.store.account(TREASURY_ACCOUNT_ID)?;
        let _replenishment = state.0.treasury_replenishment.lock().await;
        replenish_treasury(state, &seed, &treasury, 1).await?;
    }
    let funding = fund_from_treasury(
        state,
        1,
        "ironwood",
        5 * ZATOSHIS_PER_ZEC,
        INITIAL_FUNDING_KEY,
    )
    .await?;
    anyhow::ensure!(
        funding.status != "failed",
        "startup funding failed: {}",
        funding
            .last_error
            .as_deref()
            .unwrap_or("no failure reason was recorded")
    );
    if funding.status != "confirmed" {
        return Err(crate::bootstrap::ReadinessError::FundingPending.into());
    }
    let attempt = PaymentAttempt {
        state,
        dispatch_deadline: None,
    };
    let transaction = attempt
        .lookup_transaction(&funding.txid)
        .await
        .context("looking up startup funding confirmation")?;
    if !matches!(
        original_presence_from_lookup(&attempt, &funding.txid, transaction)
            .await
            .context("checking canonical startup funding confirmation")?,
        OriginalTransactionState::Included(_)
    ) {
        return Err(crate::bootstrap::ReadinessError::FundingPending.into());
    }
    attempt
        .synchronize_wallet(None)
        .await
        .context("synchronizing confirmed startup funding")?;
    // Completed provisioning survives user spending. The five-ZEC balance is
    // an initial readiness invariant, not a permanent account restriction.
    if already_provisioned {
        return Ok(());
    }
    let account = state
        .accounts()
        .await
        .into_iter()
        .find(|account| account.id == 1)
        .context("Account 1 disappeared during startup provisioning")?;
    anyhow::ensure!(
        account.ironwood_zatoshi == 5 * ZATOSHIS_PER_ZEC,
        "Account 1 startup Ironwood balance is {}, expected {} zatoshi",
        account.ironwood_zatoshi,
        5 * ZATOSHIS_PER_ZEC
    );
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/payments/treasury/tests.rs"]
pub(crate) mod tests;

pub(crate) async fn fund_from_treasury(
    state: &AppState,
    account_id: u8,
    pool: &str,
    amount_zatoshi: u64,
    idempotency_key: &str,
) -> anyhow::Result<Activity> {
    let attempt = PaymentAttempt {
        state,
        dispatch_deadline: None,
    };
    let _payment = attempt.lock(&state.0.payments).await?;
    let pending = state
        .0
        .store
        .claim_faucet(account_id, pool, amount_zatoshi, idempotency_key)?;
    let intent = crate::db::ActivityIntent {
        activity: pending.clone(),
        key: idempotency_key.to_owned(),
        memo: None,
    };
    execute_activity_intent(&attempt, &intent, pending).await
}
