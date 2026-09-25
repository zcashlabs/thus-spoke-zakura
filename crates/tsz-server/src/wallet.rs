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
use tokio::sync::Mutex;
use zcash_client_backend::{
    data_api::{
        Account as _, AccountBirthday, WalletRead, WalletWrite,
        chain::{BlockCache, BlockSource, ChainState, error},
        error::Error as WalletError,
        scanning::ScanRange,
        wallet::{
            ConfirmationsPolicy, SpendingKeys, create_proposed_transactions,
            decrypt_and_store_transaction,
            input_selection::{GreedyInputSelector, SpendPolicy, TransparentSpendPolicy},
            propose_shielding_coinbase, propose_standard_transfer_to_address, propose_transfer,
        },
    },
    fees::{DustOutputPolicy, StandardFeeRule, standard::SingleOutputChangeStrategy},
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

use crate::{db::Account, rpc::NodeRpc};

type Db = WalletDb<rusqlite::Connection, LocalNetwork, SystemClock, UnwrapErr<SysRng>>;

#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    #[error("insufficient spendable funds (have {available}, need {required} including fees)")]
    InsufficientFunds { available: u64, required: u64 },
    #[error("faucet treasury remains insufficient after replenishment")]
    TreasuryExhausted,
}

#[derive(Clone)]
pub struct RealWallet {
    db: Arc<Mutex<Db>>,
    account_ids: Vec<AccountUuid>,
    lightwalletd: String,
    chain_guard: Option<NodeRpc>,
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
        Self::open_with_birthday(
            data_dir,
            seed_hex,
            ChainState::empty(BlockHeight::from_u32(1), BlockHash([0; 32])),
        )
    }

    pub async fn open_external(data_dir: &Path, seed_hex: &str) -> Result<Self> {
        let endpoint =
            std::env::var("TSZ_LIGHTWALLETD").unwrap_or_else(|_| "http://127.0.0.1:9067".into());
        let mut client = CompactTxStreamerClient::connect(endpoint).await?;
        let state = client
            .get_tree_state(zcash_client_backend::proto::service::BlockId {
                height: 1,
                hash: vec![],
            })
            .await?
            .into_inner();
        anyhow::ensure!(
            state.height == 1,
            "lightwalletd did not return block 1's tree state"
        );
        Self::open_with_birthday(data_dir, seed_hex, state.to_chain_state()?)
    }

    fn open_with_birthday(
        data_dir: &Path,
        seed_hex: &str,
        birthday_state: ChainState,
    ) -> Result<Self> {
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

        initialize_accounts(&mut db, &secret, birthday_state)?;
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
            chain_guard: None,
        })
    }

    pub fn with_chain_guard(mut self, rpc: NodeRpc) -> Self {
        self.chain_guard = Some(rpc);
        self
    }

    async fn validate_before_submission(&self) -> Result<()> {
        if let Some(rpc) = &self.chain_guard {
            rpc.validate_network().await?;
            rpc.validate_chain_anchor().await?;
        }
        Ok(())
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
        let proposal = if source_pool == "transparent" {
            let request = TransactionRequest::new(vec![Payment::new(
                recipient.to_zcash_address(&params),
                Some(amount),
                None,
                None,
                None,
                vec![],
            )?])?;
            let selector = GreedyInputSelector::<Db>::new();
            let change = SingleOutputChangeStrategy::<Db>::new(
                StandardFeeRule::Zip317,
                None,
                ShieldedPool::Orchard,
                DustOutputPolicy::default(),
            );
            let policy = SpendPolicy::shielded_pools([])
                .with_transparent(TransparentSpendPolicy::any_account_addr());
            propose_transfer::<_, _, _, _, Infallible>(
                &mut *db,
                &params,
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
                &mut *db,
                &params,
                StandardFeeRule::Zip317,
                account_id,
                ConfirmationsPolicy::MIN,
                &recipient,
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
        self.validate_before_submission().await?;
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
        self.validate_before_submission().await?;
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

fn initialize_accounts(
    db: &mut Db,
    seed: &SecretVec<u8>,
    birthday_state: ChainState,
) -> Result<()> {
    db.transactionally(|db| -> Result<()> {
        let mut existing = db
            .get_account_ids()?
            .into_iter()
            .map(|id| db.get_account(id)?.context("wallet account disappeared"))
            .collect::<Result<Vec<_>>>()?;
        existing.sort_by(|a, b| a.name().cmp(&b.name()));
        let total = usize::from(crate::db::TREASURY_ACCOUNT_ID);
        anyhow::ensure!(
            existing.len() <= total,
            "wallet database has unexpected accounts"
        );
        let fingerprint = zip32::fingerprint::SeedFingerprint::from_seed(seed.expose_secret())
            .context("invalid wallet seed")?;
        // Recover partial wallets made by older launchers only if their accounts
        // form the expected prefix. Never silently derive different account keys.
        for (index, account) in existing.iter().enumerate() {
            let derivation = account
                .source()
                .key_derivation()
                .context("unexpected imported account")?;
            anyhow::ensure!(
                account.name() == Some(format!("Account {}", index + 1).as_str())
                    && u32::from(derivation.account_index()) as usize == index
                    && derivation.seed_fingerprint() == &fingerprint,
                "wallet accounts do not match the development seed and account sequence"
            );
        }
        // Start scanning at block 2 using the real (external) or empty (managed)
        // block-1 state. All missing accounts commit together or not at all.
        let birthday = AccountBirthday::from_parts(birthday_state, None);
        for id in (existing.len() + 1)..=total {
            db.create_account(&format!("Account {id}"), seed, &birthday, None)?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::testing::{MockRpc, regtest_reply};
    use serde_json::json;

    #[tokio::test]
    async fn transaction_submission_guard_rejects_a_replaced_external_node() {
        let server = MockRpc::start(|request| {
            if request["method"] == "getblockhash" && request["params"][0] == 1 {
                Ok(json!("replacement-anchor"))
            } else {
                regtest_reply(request)
            }
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let wallet = RealWallet::open(dir.path(), &hex::encode([42; 64]))
            .unwrap()
            .with_chain_guard(
                server
                    .rpc
                    .clone()
                    .with_chain_anchor("original-anchor".into()),
            );
        assert!(
            wallet
                .validate_before_submission()
                .await
                .unwrap_err()
                .to_string()
                .contains("external chain changed")
        );
    }

    fn empty_birthday() -> ChainState {
        ChainState::empty(BlockHeight::from_u32(1), BlockHash([0; 32]))
    }

    fn initialized_db(dir: &Path, seed: &SecretVec<u8>) -> Db {
        let mut db = WalletDb::for_path(
            dir.join("wallet.db"),
            regtest_network(),
            SystemClock,
            UnwrapErr(SysRng),
        )
        .unwrap();
        init_wallet_db(&mut db, Some(SecretVec::new(seed.expose_secret().clone()))).unwrap();
        db
    }

    #[test]
    fn resumes_each_partial_account_prefix_without_replacing_keys() {
        let seed = SecretVec::new(vec![42; 64]);
        for count in 1..=5 {
            let dir = tempfile::tempdir().unwrap();
            let mut db = initialized_db(dir.path(), &seed);
            let birthday = AccountBirthday::from_parts(empty_birthday(), None);
            let mut old_ids = vec![];
            for id in 1..=count {
                old_ids.push(
                    db.create_account(&format!("Account {id}"), &seed, &birthday, None)
                        .unwrap()
                        .0,
                );
            }
            drop(db);
            let wallet = RealWallet::open_with_birthday(
                dir.path(),
                &hex::encode(seed.expose_secret()),
                empty_birthday(),
            )
            .unwrap();
            assert_eq!(wallet.account_ids.len(), 6);
            assert_eq!(&wallet.account_ids[..count], &old_ids);
            let ids = wallet.account_ids.clone();
            drop(wallet);
            let reopened = RealWallet::open_with_birthday(
                dir.path(),
                &hex::encode(seed.expose_secret()),
                empty_birthday(),
            )
            .unwrap();
            assert_eq!(reopened.account_ids, ids);
        }
    }

    #[test]
    fn account_initialization_rolls_back_the_entire_batch_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let seed = SecretVec::new(vec![42; 64]);
        let mut db = initialized_db(dir.path(), &seed);
        let connection = rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap();
        connection.execute_batch("CREATE TRIGGER fail_second_account BEFORE INSERT ON accounts WHEN NEW.name = 'Account 2' BEGIN SELECT RAISE(ABORT, 'injected account failure'); END;").unwrap();
        assert!(initialize_accounts(&mut db, &seed, empty_birthday()).is_err());
        assert!(db.get_account_ids().unwrap().is_empty());
        connection
            .execute_batch("DROP TRIGGER fail_second_account;")
            .unwrap();
        initialize_accounts(&mut db, &seed, empty_birthday()).unwrap();
        assert_eq!(db.get_account_ids().unwrap().len(), 6);
    }

    #[test]
    fn refuses_to_extend_an_unexpected_partial_wallet() {
        let dir = tempfile::tempdir().unwrap();
        let seed = SecretVec::new(vec![42; 64]);
        let mut db = initialized_db(dir.path(), &seed);
        let birthday = AccountBirthday::from_parts(empty_birthday(), None);
        db.create_account("Account 2", &seed, &birthday, None)
            .unwrap();
        assert!(initialize_accounts(&mut db, &seed, empty_birthday()).is_err());
        assert_eq!(db.get_account_ids().unwrap().len(), 1);
    }
}
