use std::{
    collections::BTreeMap,
    convert::Infallible,
    io,
    path::Path,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use rand10::{rand_core::UnwrapErr, rngs::SysRng};
use secrecy::{ExposeSecret, SecretVec};
use serde::Serialize;
use tokio::sync::Mutex;
use zcash_client_backend::{
    data_api::{
        Account as _, AccountBirthday, CoinbaseFilter, InputSource, MaxSpendMode, TargetValue,
        WalletRead, WalletWrite,
        chain::{BlockCache, BlockSource, ChainState, error},
        error::Error as WalletError,
        scanning::ScanRange,
        wallet::{
            ConfirmationsPolicy, SpendingKeys, create_proposed_transactions,
            decrypt_and_store_transaction,
            input_selection::{
                GreedyInputSelector, LockFilter, LockedInputPolicy, SpendPolicy,
                TransparentSpendPolicy,
            },
            propose_send_max_transfer, propose_shielding_coinbase,
            propose_standard_transfer_to_address, propose_transfer,
        },
    },
    fees::{DustOutputPolicy, StandardFeeRule, standard::SingleOutputChangeStrategy},
    proposal::Proposal,
    proto::service::compact_tx_streamer_client::CompactTxStreamerClient,
    proto::{
        compact_formats::CompactBlock,
        service::{ChainSpec, RawTransaction},
    },
    sync,
    wallet::OvkPolicy,
};
use zcash_client_sqlite::{AccountUuid, WalletDb, util::SystemClock, wallet::init::init_wallet_db};
use zcash_keys::{address::Address, keys::UnifiedSpendingKey};
use zcash_primitives::block::BlockHash;
use zcash_primitives::transaction::Transaction;
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::{
    ShieldedPool,
    consensus::{BlockHeight, BranchId},
    local_consensus::LocalNetwork,
    value::Zatoshis,
};
use zip321::{Payment, TransactionRequest};

use crate::db::{Account, ZATOSHIS_PER_ZEC};

type Db = WalletDb<rusqlite::Connection, LocalNetwork, SystemClock, UnwrapErr<SysRng>>;

#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    #[error(
        "insufficient spendable funds (have {}, need {} including fees)",
        format_zec(*available),
        format_zec(*required)
    )]
    InsufficientFunds { available: u64, required: u64 },
    #[error("faucet treasury remains insufficient after replenishment")]
    TreasuryExhausted,
}

/// Spendable balance, fee, and maximum sendable amount for emptying one pool.
#[derive(Serialize)]
pub struct SendQuote {
    pub available_zatoshi: u64,
    pub fee_zatoshi: u64,
    pub max_zatoshi: u64,
}

fn format_zec(zatoshi: u64) -> String {
    let whole = zatoshi / ZATOSHIS_PER_ZEC;
    let fraction = zatoshi % ZATOSHIS_PER_ZEC;
    if fraction == 0 {
        format!("{whole} ZEC")
    } else {
        format!(
            "{whole}.{} ZEC",
            format!("{fraction:08}").trim_end_matches('0')
        )
    }
}

#[derive(Clone)]
pub struct RealWallet {
    db: Arc<Mutex<Db>>,
    account_ids: Vec<AccountUuid>,
    lightwalletd: String,
}

#[derive(Default)]
struct MemoryBlockCache(StdMutex<BTreeMap<u32, CompactBlock>>);

impl BlockSource for MemoryBlockCache {
    type Error = io::Error;

    fn with_blocks<F, E>(
        &self,
        from: Option<BlockHeight>,
        limit: Option<usize>,
        mut f: F,
    ) -> Result<(), error::Error<E, Self::Error>>
    where
        F: FnMut(CompactBlock) -> Result<(), error::Error<E, Self::Error>>,
    {
        let from = from.map(u32::from).unwrap_or(0);
        for block in self
            .0
            .lock()
            .unwrap()
            .range(from..)
            .take(limit.unwrap_or(usize::MAX))
        {
            f(block.1.clone())?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl BlockCache for MemoryBlockCache {
    fn get_tip_height(
        &self,
        range: Option<&ScanRange>,
    ) -> Result<Option<BlockHeight>, Self::Error> {
        let blocks = self.0.lock().unwrap();
        Ok(blocks
            .values()
            .rev()
            .find(|block| {
                range.is_none_or(|r| {
                    r.block_range()
                        .contains(&BlockHeight::from_u32(block.height as u32))
                })
            })
            .map(|b| BlockHeight::from_u32(b.height as u32)))
    }
    async fn read(&self, range: &ScanRange) -> Result<Vec<CompactBlock>, Self::Error> {
        let start = u32::from(range.block_range().start);
        let end = u32::from(range.block_range().end);
        Ok(self
            .0
            .lock()
            .unwrap()
            .range(start..end)
            .map(|(_, b)| b.clone())
            .collect())
    }
    async fn insert(&self, blocks: Vec<CompactBlock>) -> Result<(), Self::Error> {
        let mut cache = self.0.lock().unwrap();
        for block in blocks {
            cache.insert(block.height as u32, block);
        }
        Ok(())
    }
    async fn delete(&self, range: ScanRange) -> Result<(), Self::Error> {
        let start = u32::from(range.block_range().start);
        let end = u32::from(range.block_range().end);
        self.0
            .lock()
            .unwrap()
            .retain(|height, _| !(*height >= start && *height < end));
        Ok(())
    }
}

pub fn regtest_network() -> LocalNetwork {
    let one = Some(BlockHeight::from_u32(1));
    LocalNetwork {
        overwinter: one,
        sapling: one,
        blossom: one,
        heartwood: one,
        canopy: one,
        nu5: one,
        nu6: one,
        nu6_1: None,
        nu6_2: None,
        nu6_3: None,
    }
}

impl RealWallet {
    pub fn open(data_dir: &Path, seed_hex: &str) -> Result<Self> {
        let seed = hex::decode(seed_hex).context("invalid wallet seed")?;
        let secret = SecretVec::new(seed);
        let wallet_path = data_dir.join("wallet.db");
        let mut db = WalletDb::for_path(
            wallet_path,
            regtest_network(),
            SystemClock,
            UnwrapErr(SysRng),
        )?;
        init_wallet_db(
            &mut db,
            Some(SecretVec::new(secret.expose_secret().clone())),
        )
        .map_err(|e| anyhow::anyhow!("initializing wallet database: {e}"))?;

        let account_count = db.get_account_ids()?.len();
        if account_count == 0 || account_count == usize::from(crate::db::USER_ACCOUNT_COUNT) {
            // lightwalletd treats a BlockId with height 0 as unspecified, while the
            // SDK asks for the tree state immediately before an account birthday.
            // Start at block 2 so that the initial tree-state request is for block 1.
            // Block 1 is an expendable mining-reward block on this local regtest.
            let birthday = AccountBirthday::from_parts(
                ChainState::empty(BlockHeight::from_u32(1), BlockHash([0; 32])),
                None,
            );
            for id in (account_count + 1)..=usize::from(crate::db::TREASURY_ACCOUNT_ID) {
                db.create_account(&format!("Account {id}"), &secret, &birthday, None)?;
            }
        }
        if db.get_account_ids()?.len() != usize::from(crate::db::TREASURY_ACCOUNT_ID) {
            bail!("wallet database must contain exactly six accounts");
        }
        let mut accounts = db
            .get_account_ids()?
            .into_iter()
            .map(|id| {
                db.get_account(id)?
                    .map(|account| (account.name().unwrap_or_default().to_owned(), id))
                    .context("wallet account disappeared")
            })
            .collect::<Result<Vec<_>>>()?;
        accounts.sort_by(|a, b| a.0.cmp(&b.0));
        let account_ids = accounts.into_iter().map(|(_, id)| id).collect();

        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            account_ids,
            lightwalletd: std::env::var("TSZ_LIGHTWALLETD")
                .unwrap_or_else(|_| "http://127.0.0.1:9067".into()),
        })
    }

    pub async fn sync(&self) -> Result<()> {
        let cache = MemoryBlockCache::default();
        let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
        let mut db = self.db.lock().await;
        sync::run(&mut client, &regtest_network(), &cache, &mut *db, 100)
            .await
            .map_err(|e| anyhow::anyhow!("wallet sync failed: {e}"))
    }

    pub async fn wait_for_height(&self, target: u64, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
        loop {
            let indexed = client
                .get_latest_block(ChainSpec::default())
                .await?
                .into_inner()
                .height;
            if indexed >= target {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "lightwalletd did not index Zakura height {target} within {} seconds (latest indexed height: {indexed})",
                    timeout.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    pub async fn latest_height(&self) -> Result<u64> {
        let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
        Ok(client
            .get_latest_block(ChainSpec::default())
            .await?
            .into_inner()
            .height)
    }

    pub async fn heights(&self) -> Result<(Option<u64>, Option<u64>)> {
        let db = self.db.lock().await;
        Ok(db
            .get_wallet_summary(ConfirmationsPolicy::MIN)?
            .map(|summary| {
                (
                    Some(u64::from(u32::from(summary.fully_scanned_height()))),
                    Some(u64::from(u32::from(summary.chain_tip_height()))),
                )
            })
            .unwrap_or((None, None)))
    }

    pub async fn apply_balances(&self, accounts: &mut [Account]) -> Result<()> {
        let db = self.db.lock().await;
        let Some(summary) = db.get_wallet_summary(ConfirmationsPolicy::MIN)? else {
            return Ok(());
        };
        for (account, wallet_id) in accounts.iter_mut().zip(&self.account_ids) {
            if let Some(balance) = summary.account_balances().get(wallet_id) {
                account.transparent_zatoshi = u64::from(balance.unshielded_balance().total());
                account.orchard_zatoshi = u64::from(balance.orchard_balance().total());
            }
        }
        Ok(())
    }

    pub async fn enhance_transaction(&self, raw_hex: &str, height: u32) -> Result<()> {
        let params = regtest_network();
        let height = BlockHeight::from_u32(height);
        let raw = hex::decode(raw_hex).context("invalid transaction hex from Zakura")?;
        let tx = Transaction::read(&raw[..], BranchId::for_height(&params, height))
            .context("parsing transaction from Zakura")?;
        let mut db = self.db.lock().await;
        decrypt_and_store_transaction(&params, &mut *db, &tx, Some(height))?;
        Ok(())
    }

    /// Builds the proposal a send would use, without signing or broadcasting.
    fn propose_send<DbT>(
        db: &mut DbT,
        params: &LocalNetwork,
        account_id: <DbT as InputSource>::AccountId,
        source_pool: &str,
        recipient: &Address,
        amount: Zatoshis,
    ) -> Result<Proposal<StandardFeeRule, <DbT as InputSource>::NoteRef>>
    where
        DbT: WalletWrite
            + InputSource<Error = <DbT as WalletRead>::Error>
            + WalletRead<AccountId = <DbT as InputSource>::AccountId>,
        <DbT as InputSource>::NoteRef: Copy + Eq + Ord + std::fmt::Display,
        <DbT as WalletRead>::Error: std::error::Error + Send + Sync + 'static,
    {
        let proposal = if source_pool == "transparent" {
            let request = TransactionRequest::new(vec![Payment::new(
                recipient.to_zcash_address(params),
                Some(amount),
                None,
                None,
                None,
                vec![],
            )?])?;
            let selector = GreedyInputSelector::<DbT>::new();
            let change = SingleOutputChangeStrategy::<DbT>::new(
                StandardFeeRule::Zip317,
                None,
                ShieldedPool::Orchard,
                DustOutputPolicy::default(),
            );
            let policy = SpendPolicy::shielded_pools([])
                .with_transparent(TransparentSpendPolicy::any_account_addr());
            propose_transfer::<_, _, _, _, Infallible>(
                db,
                params,
                account_id,
                &selector,
                &change,
                request,
                ConfirmationsPolicy::MIN,
                &policy,
                None,
                None,
            )
        } else if source_pool == "orchard" {
            propose_standard_transfer_to_address::<_, _, Infallible>(
                db,
                params,
                StandardFeeRule::Zip317,
                account_id,
                ConfirmationsPolicy::MIN,
                recipient,
                amount,
                None,
                None,
                ShieldedPool::Orchard,
                None,
                None,
            )
        } else {
            bail!("source pool must be transparent or orchard")
        }
        .map_err(|error| match error {
            WalletError::InsufficientFunds {
                available,
                required,
            } => anyhow::Error::new(PaymentError::InsufficientFunds {
                available: u64::from(available),
                required: u64::from(required),
            }),
            error => anyhow::anyhow!("proposing {source_pool} transaction: {error}"),
        })?;
        Ok(proposal)
    }

    pub async fn send(
        &self,
        seed_hex: &str,
        from_account: u8,
        source_pool: &str,
        destination: &str,
        amount: u64,
    ) -> Result<String> {
        let account_index = from_account
            .checked_sub(1)
            .context("invalid source account")?;
        let mut db = self.db.lock().await;
        let account_id = *self
            .account_ids
            .get(account_index as usize)
            .context("source account does not exist")?;
        let params = regtest_network();
        let recipient =
            Address::decode(&params, destination).context("invalid destination address")?;
        let amount = Zatoshis::from_u64(amount).map_err(|_| anyhow::anyhow!("invalid amount"))?;
        let proposal = Self::propose_send(
            &mut *db,
            &params,
            account_id,
            source_pool,
            &recipient,
            amount,
        )?;
        let seed = hex::decode(seed_hex)?;
        let usk = UnifiedSpendingKey::from_seed(
            &params,
            &seed,
            zip32::AccountId::try_from(u32::from(account_index))
                .map_err(|_| anyhow::anyhow!("invalid account index"))?,
        )
        .map_err(|e| anyhow::anyhow!("deriving spending key: {e:?}"))?;
        let prover = LocalTxProver::bundled();
        let txids = create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
            &mut *db,
            &params,
            &prover,
            &prover,
            &SpendingKeys::from_unified_spending_key(usk),
            OvkPolicy::Sender,
            &proposal,
            None,
        )
        .map_err(|e| anyhow::anyhow!("building transaction: {e}"))?;
        let txid = *txids.first();
        let tx = db
            .get_transaction(txid)?
            .context("built transaction was not stored")?;
        let mut raw = vec![];
        tx.write(&mut raw)?;
        drop(db);
        let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
        let result = client
            .send_transaction(RawTransaction {
                data: raw,
                height: 0,
            })
            .await?
            .into_inner();
        if result.error_code != 0 {
            bail!(
                "lightwalletd rejected transaction: {}",
                result.error_message
            );
        }
        Ok(txid.to_string())
    }

    /// Returns the exact fee and maximum spendable amount for emptying a pool.
    pub async fn send_quote(
        &self,
        from_account: u8,
        source_pool: &str,
        destination: &str,
    ) -> Result<SendQuote> {
        let account_index = from_account
            .checked_sub(1)
            .context("invalid source account")?;
        let mut db = self.db.lock().await;
        let account_id = *self
            .account_ids
            .get(account_index as usize)
            .context("source account does not exist")?;
        let params = regtest_network();
        let recipient =
            Address::decode(&params, destination).context("invalid destination address")?;
        Self::quote(&mut *db, &params, account_id, source_pool, &recipient)
    }

    fn quote<DbT>(
        db: &mut DbT,
        params: &LocalNetwork,
        account_id: <DbT as InputSource>::AccountId,
        source_pool: &str,
        recipient: &Address,
    ) -> Result<SendQuote>
    where
        DbT: WalletWrite
            + InputSource<Error = <DbT as WalletRead>::Error>
            + WalletRead<AccountId = <DbT as InputSource>::AccountId>,
        <DbT as InputSource>::NoteRef: Copy + Eq + Ord + std::fmt::Display,
        <DbT as WalletRead>::Error: std::error::Error + Send + Sync + 'static,
    {
        if source_pool == "orchard" {
            match propose_send_max_transfer::<_, _, _, Infallible>(
                db,
                params,
                account_id,
                &[ShieldedPool::Orchard],
                &StandardFeeRule::Zip317,
                recipient.to_zcash_address(params),
                None,
                MaxSpendMode::MaxSpendable,
                ConfirmationsPolicy::MIN,
                &LockedInputPolicy::Exclude,
                None,
            ) {
                Ok(proposal) => {
                    let fee = proposal
                        .steps()
                        .iter()
                        .try_fold(0u64, |total, step| {
                            total.checked_add(u64::from(step.balance().fee_required()))
                        })
                        .context("transaction fee exceeds the maximum money supply")?;
                    let max = u64::from(
                        proposal
                            .steps()
                            .first()
                            .transaction_request()
                            .payments()
                            .values()
                            .next()
                            .and_then(|payment| payment.amount())
                            .unwrap_or(Zatoshis::ZERO),
                    );
                    Ok(SendQuote {
                        available_zatoshi: max + fee,
                        fee_zatoshi: fee,
                        max_zatoshi: max,
                    })
                }
                Err(WalletError::InsufficientFunds {
                    available,
                    required,
                }) => Ok(SendQuote {
                    available_zatoshi: u64::from(available),
                    fee_zatoshi: u64::from(required).saturating_sub(1),
                    max_zatoshi: 0,
                }),
                Err(error) => Err(anyhow::anyhow!("proposing max spend: {error}")),
            }
        } else if source_pool == "transparent" {
            let (target_height, _) = db
                .get_target_and_anchor_heights(ConfirmationsPolicy::MIN.trusted())?
                .context("wallet has not scanned any blocks yet")?;
            let spendable = db
                .select_spendable_transparent_outputs(
                    account_id,
                    target_height,
                    ConfirmationsPolicy::MIN,
                    CoinbaseFilter::NonCoinbaseOnly,
                    None,
                    TargetValue::AllFunds(MaxSpendMode::MaxSpendable),
                    usize::MAX,
                    &StandardFeeRule::Zip317,
                    LockFilter::Policy(&LockedInputPolicy::Exclude),
                )?
                .into_iter()
                .try_fold(0u64, |total, utxo| {
                    total.checked_add(u64::from(utxo.value()))
                })
                .context("transparent balance exceeds the maximum money supply")?;
            // There is no send-max proposal for transparent inputs, but the
            // failed proposal still reports the fee for spending them all.
            let probe_amount = spendable.max(1);
            let probe = Self::propose_send(
                db,
                params,
                account_id,
                source_pool,
                recipient,
                Zatoshis::from_u64(probe_amount).map_err(|_| anyhow::anyhow!("invalid amount"))?,
            );
            match probe {
                Err(error) => match error.downcast_ref() {
                    Some(PaymentError::InsufficientFunds { required, .. }) => {
                        let fee = required.saturating_sub(probe_amount);
                        Ok(SendQuote {
                            available_zatoshi: spendable,
                            fee_zatoshi: fee,
                            max_zatoshi: spendable.saturating_sub(fee),
                        })
                    }
                    _ => Err(error),
                },
                Ok(_) => bail!("a send of the entire balance proposed cleanly"),
            }
        } else {
            bail!("source pool must be transparent or orchard")
        }
    }

    pub async fn shield_coinbase(
        &self,
        seed_hex: &str,
        treasury_account: u8,
        from: &str,
        to: &str,
    ) -> Result<String> {
        let mut db = self.db.lock().await;
        let params = regtest_network();
        let from = match Address::decode(&params, from).context("invalid treasury address")? {
            Address::Transparent(address) => address,
            _ => bail!("treasury address is not transparent"),
        };
        let to = Address::decode(&params, to)
            .context("invalid shielding address")?
            .to_zcash_address(&params);
        let proposal = propose_shielding_coinbase::<_, _, _, _, Infallible>(
            &mut *db,
            &params,
            &GreedyInputSelector::new(),
            &StandardFeeRule::Zip317,
            Zatoshis::ZERO,
            &[from],
            to,
            None,
            None,
            None,
        )
        .map_err(|e| match e {
            WalletError::InsufficientFunds { .. } => {
                anyhow::Error::new(PaymentError::TreasuryExhausted)
            }
            e => anyhow::anyhow!("proposing coinbase shielding: {e}"),
        })?;
        let seed = hex::decode(seed_hex)?;
        let account_index = treasury_account
            .checked_sub(1)
            .context("invalid treasury account")?;
        let usk = UnifiedSpendingKey::from_seed(
            &params,
            &seed,
            zip32::AccountId::try_from(u32::from(account_index))
                .map_err(|_| anyhow::anyhow!("invalid treasury account"))?,
        )
        .map_err(|e| anyhow::anyhow!("deriving treasury key: {e:?}"))?;
        let prover = LocalTxProver::bundled();
        let txids = create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
            &mut *db,
            &params,
            &prover,
            &prover,
            &SpendingKeys::from_unified_spending_key(usk),
            OvkPolicy::Sender,
            &proposal,
            None,
        )
        .map_err(|e| anyhow::anyhow!("building shielding transaction: {e}"))?;
        let txid = *txids.first();
        let tx = db
            .get_transaction(txid)?
            .context("shielding transaction was not stored")?;
        let mut raw = vec![];
        tx.write(&mut raw)?;
        drop(db);
        let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
        let response = client
            .send_transaction(RawTransaction {
                data: raw,
                height: 0,
            })
            .await?
            .into_inner();
        if response.error_code != 0 {
            bail!(
                "lightwalletd rejected shielding: {}",
                response.error_message
            );
        }
        Ok(txid.to_string())
    }
}

#[cfg(test)]
mod tests {
    use zcash_client_backend::{
        data_api::testing::{
            orchard::OrchardPoolTester,
            pool::dsl::{TestDsl, TestScenario},
        },
        wallet::WalletTransparentOutput,
    };
    use zcash_client_sqlite::testing::{BlockCache, db::TestDbFactory};
    use zcash_keys::keys::UnifiedAddressRequest;
    use zcash_transparent::{
        bundle::{OutPoint, TxOut},
        keys::TransparentKeyScope,
    };

    use super::*;

    type TestState = TestDsl<TestScenario<OrchardPoolTester, BlockCache, TestDbFactory>>;

    fn test_state() -> TestState {
        TestDsl::with_sapling_birthday_account(TestDbFactory::default(), BlockCache::new())
            .build::<OrchardPoolTester>()
    }

    fn recipient(st: &TestState) -> Address {
        let account_id = st.test_account().unwrap().id();
        Address::Unified(
            st.wallet()
                .get_last_generated_address_matching(
                    account_id,
                    UnifiedAddressRequest::AllAvailableKeys,
                )
                .unwrap()
                .unwrap(),
        )
    }

    fn quote(st: &mut TestState, source_pool: &str) -> Result<SendQuote> {
        let recipient = recipient(st);
        let account_id = st.test_account().unwrap().id();
        let params = st.wallet().db().params().clone();
        RealWallet::quote(
            st.wallet_mut(),
            &params,
            account_id,
            source_pool,
            &recipient,
        )
    }

    fn fund_transparent(st: &mut TestState, value: u64) {
        st.add_empty_blocks(10);
        let account_id = st.test_account().unwrap().id();
        let taddr = match recipient(st) {
            Address::Unified(ref ua) => *ua.transparent().unwrap(),
            _ => unreachable!(),
        };
        let utxo = WalletTransparentOutput::from_parts(
            OutPoint::new([1; 32], 0),
            TxOut::new(Zatoshis::const_from_u64(value), taddr.script().into()),
            Some(st.wallet().chain_height().unwrap().unwrap()),
            Some(account_id),
            Some(TransparentKeyScope::EXTERNAL),
            None,
        )
        .unwrap();
        st.wallet_mut()
            .put_received_transparent_utxo(&utxo)
            .unwrap();
    }

    #[test]
    fn orchard_quote_reports_fee_and_max() {
        let mut st = test_state();
        st.add_notes_checking_balance([[Zatoshis::const_from_u64(100_000_000)]]);
        let quote = quote(&mut st, "orchard").unwrap();
        assert_eq!(quote.available_zatoshi, 100_000_000);
        assert_eq!(quote.fee_zatoshi, 10_000);
        assert_eq!(quote.max_zatoshi, 99_990_000);
    }

    #[test]
    fn orchard_quote_max_is_zero_when_balance_only_covers_the_fee() {
        let mut st = test_state();
        st.add_notes_checking_balance([[Zatoshis::const_from_u64(10_000)]]);
        let quote = quote(&mut st, "orchard").unwrap();
        assert_eq!(quote.available_zatoshi, 10_000);
        assert_eq!(quote.fee_zatoshi, 10_000);
        assert_eq!(quote.max_zatoshi, 0);
    }

    #[test]
    fn transparent_quote_reports_fee_and_max() {
        let mut st = test_state();
        fund_transparent(&mut st, 100_000_000);
        let quote = quote(&mut st, "transparent").unwrap();
        assert_eq!(quote.available_zatoshi, 100_000_000);
        assert!(quote.fee_zatoshi > 0);
        assert_eq!(quote.max_zatoshi, 100_000_000 - quote.fee_zatoshi);
    }

    #[test]
    fn transparent_quote_max_is_zero_when_balance_is_below_the_fee() {
        let mut st = test_state();
        fund_transparent(&mut st, 9_000);
        let quote = quote(&mut st, "transparent").unwrap();
        assert_eq!(quote.available_zatoshi, 9_000);
        assert!(quote.fee_zatoshi >= 9_000);
        assert_eq!(quote.max_zatoshi, 0);
    }

    #[test]
    fn transparent_quote_reports_zero_max_for_dust() {
        let mut st = test_state();
        fund_transparent(&mut st, 5_000);
        let quote = quote(&mut st, "transparent").unwrap();
        assert_eq!(quote.available_zatoshi, 0);
        assert!(quote.fee_zatoshi > 0);
        assert_eq!(quote.max_zatoshi, 0);
    }
}
