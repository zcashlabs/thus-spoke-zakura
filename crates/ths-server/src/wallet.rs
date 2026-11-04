use std::{
    collections::{BTreeMap, HashSet},
    convert::Infallible,
    io,
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use rand10::{rand_core::UnwrapErr, rngs::SysRng};
use rusqlite::OptionalExtension;
use schemerz_rusqlite::RusqliteMigration;
use secrecy::{ExposeSecret, SecretVec};
use serde::Serialize;
use tokio::sync::Mutex;
use uuid::Uuid;
use zcash_client_backend::{
    data_api::{
        Account as _, AccountBirthday, CoinbaseFilter, InputSource, MaxSpendMode, TargetValue,
        WalletCommitmentTrees, WalletRead, WalletWrite,
        chain::{BlockSource, ChainState, CommitmentTreeRoot, error, scan_cached_blocks},
        enhance_pir::{EnhancePirRead, EnhancementMode, TransactionEnhancementWork},
        error::Error as WalletError,
        scanning::ScanRange,
        transparent_ledger::TransparentLedgerMode,
        wallet::{
            ConfirmationsPolicy, SpendingKeys, create_proposed_transactions,
            decrypt_and_store_transaction,
            input_selection::{
                GreedyInputSelector, LockFilter, LockedInputPolicy, SpendPolicy,
                TransparentSpendPolicy,
            },
            propose_send_max_transfer, propose_shielding_coinbase, propose_transfer,
        },
    },
    fees::{DustOutputPolicy, StandardFeeRule, standard::SingleOutputChangeStrategy},
    proposal::Proposal,
    proto::{
        compact_formats::CompactBlock,
        service::{
            BlockId, ChainSpec, RawTransaction, SendResponse,
            compact_tx_streamer_client::CompactTxStreamerClient,
        },
    },
    wallet::{OvkPolicy, WalletTransparentOutput},
};
use zcash_client_sqlite::{
    AccountUuid, WalletDb,
    util::SystemClock,
    wallet::init::{WalletMigrationError, WalletMigrator, migrations},
};
use zcash_keys::{address::Address, encoding::AddressCodec, keys::UnifiedSpendingKey};
use zcash_primitives::block::BlockHash;
use zcash_primitives::merkle_tree::HashSer;
use zcash_primitives::transaction::Transaction;
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::{
    ShieldedPool, TxId,
    consensus::{BlockHeight, BranchId},
    local_consensus::LocalNetwork,
    memo::MemoBytes,
    value::Zatoshis,
};
use zip321::{Payment, TransactionRequest};

use crate::db::{Account, ZATOSHIS_PER_ZEC};
use transparent::{
    address::Script,
    bundle::{OutPoint, TxOut},
};
use zcash_script::script;

use crate::{network::regtest_network, rpc::ChainCheckpoint};

mod history;
mod journal;
mod payments;
mod recovery;
mod submission;
mod transparent_history;

pub(crate) use transparent_history::{HistoryQuery, HistoryTransaction};

pub(crate) use history::{CanonicalActivity, direction as payment_direction};
pub use journal::PreparedPayment;
use journal::{PreparedPaymentsMigration, prepared_payment};
pub use payments::{PaymentError, SendPayment, SendQuote};
#[cfg(test)]
pub(crate) use submission::check_broadcast_response;

type Db = WalletDb<rusqlite::Connection, LocalNetwork, SystemClock, UnwrapErr<SysRng>>;
pub(crate) const WALLET_BIRTHDAY_HEIGHT: u32 = 2;

#[derive(Clone, Debug)]
pub(crate) struct TransparentQuery {
    pub account_id: AccountUuid,
    pub addresses: Vec<String>,
    pub start_height: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct TransparentOutputData {
    pub txid: [u8; 32],
    pub index: u32,
    pub value_zatoshi: i64,
    pub script: Vec<u8>,
    pub height: u32,
    pub account_id: AccountUuid,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ShieldedProtocol {
    Sapling,
    Orchard,
    Ironwood,
}

#[derive(Clone, Debug)]
pub(crate) struct SubtreeRootData {
    pub completing_height: u32,
    pub root_hash: Vec<u8>,
}

fn to_roots<H: HashSer>(roots: Vec<SubtreeRootData>) -> Result<Vec<CommitmentTreeRoot<H>>> {
    roots
        .into_iter()
        .map(|root| {
            Ok(CommitmentTreeRoot::from_parts(
                BlockHeight::from_u32(root.completing_height),
                H::read(&root.root_hash[..])?,
            ))
        })
        .collect()
}

pub(crate) enum ScanOutcome {
    Scanned(bool),
    Rewind(u32),
}

#[derive(Clone)]
pub struct RealWallet {
    db: Arc<Mutex<Db>>,
    account_ids: Vec<AccountUuid>,
    lightwalletd: String,
    wallet_path: std::path::PathBuf,
}

struct MemoryBlockCache(BTreeMap<u32, CompactBlock>);

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
        for block in self.0.range(from..).take(limit.unwrap_or(usize::MAX)) {
            f(block.1.clone())?;
        }
        Ok(())
    }
}

fn validate_lightwalletd_endpoint(endpoint: &str) -> Result<()> {
    let url = reqwest::Url::parse(endpoint).context("invalid THS_LIGHTWALLETD")?;
    anyhow::ensure!(
        url.scheme() == "http" && url.host_str().is_some(),
        "invalid THS_LIGHTWALLETD: expected an HTTP endpoint"
    );
    tonic::transport::Endpoint::from_shared(endpoint.to_owned())
        .context("invalid THS_LIGHTWALLETD")?;
    Ok(())
}

impl RealWallet {
    pub fn open(data_dir: &Path, seed_hex: &str) -> Result<Self> {
        let seed = hex::decode(seed_hex).context("invalid wallet seed")?;
        let secret = SecretVec::new(seed);
        let wallet_path = data_dir.join("wallet.db");
        let mut db = WalletDb::for_path(
            &wallet_path,
            regtest_network(),
            SystemClock,
            UnwrapErr(SysRng),
        )?
        .with_enhancement_mode(EnhancementMode::Standard)
        .with_transparent_ledger_mode(TransparentLedgerMode::Public);
        WalletMigrator::new()
            .with_seed(SecretVec::new(secret.expose_secret().clone()))
            .with_external_migrations(vec![
                Box::new(PreparedPaymentsMigration),
                Box::new(recovery::TreasuryCursorMigration),
                Box::new(transparent_history::TransparentHistoryMigration),
            ])
            .init_or_migrate(&mut db)
            .map_err(|e| anyhow::anyhow!("initializing wallet database: {e}"))?;

        let account_count = db.get_account_ids()?.len();
        if account_count == 0 || account_count == usize::from(crate::db::USER_ACCOUNT_COUNT) {
            // lightwalletd treats a BlockId with height 0 as unspecified, while the
            // SDK asks for the tree state immediately before an account birthday.
            // Start at block 2 so that the initial tree-state request is for block 1.
            // Block 1 is an expendable mining-reward block on this local regtest.
            let birthday = AccountBirthday::from_parts(
                ChainState::empty(
                    BlockHeight::from_u32(WALLET_BIRTHDAY_HEIGHT - 1),
                    BlockHash([0; 32]),
                ),
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

        let lightwalletd =
            std::env::var("THS_LIGHTWALLETD").unwrap_or_else(|_| "http://127.0.0.1:9067".into());
        validate_lightwalletd_endpoint(&lightwalletd)?;
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            account_ids,
            lightwalletd,
            wallet_path,
        })
    }

    pub(crate) fn lightwalletd(&self) -> &str {
        &self.lightwalletd
    }

    #[cfg(test)]
    pub(crate) fn with_lightwalletd(mut self, endpoint: String) -> Self {
        self.lightwalletd = endpoint;
        self
    }

    async fn with_db<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Db) -> Result<T> + Send + 'static,
    {
        let mut db = Arc::clone(&self.db).lock_owned().await;
        tokio::task::spawn_blocking(move || operation(&mut db))
            .await
            .context("wallet worker stopped")?
    }

    pub(crate) async fn public_transparent_queries(&self) -> Result<Vec<TransparentQuery>> {
        let account_ids = self.account_ids[..usize::from(crate::db::USER_ACCOUNT_COUNT)].to_vec();
        self.with_db(move |db| {
            account_ids
                .into_iter()
                .map(|account_id| {
                    Ok(TransparentQuery {
                        account_id,
                        addresses: db
                            .get_transparent_receivers(account_id, true, true)?
                            .into_keys()
                            .map(|address| address.encode(&regtest_network()))
                            .collect(),
                        start_height: u64::from(u32::from(db.utxo_query_height(account_id)?)),
                    })
                })
                .collect()
        })
        .await
    }

    pub(crate) async fn insert_transparent_outputs(
        &self,
        outputs: Vec<TransparentOutputData>,
    ) -> Result<()> {
        self.with_db(move |db| {
            for output in outputs {
                let output = WalletTransparentOutput::from_parts(
                    OutPoint::new(output.txid, output.index),
                    TxOut::new(
                        Zatoshis::from_nonnegative_i64(output.value_zatoshi)
                            .map_err(|_| anyhow::anyhow!("negative transparent output value"))?,
                        Script(script::Code(output.script)),
                    ),
                    Some(BlockHeight::from_u32(output.height)),
                    Some(output.account_id),
                    None,
                    None,
                )
                .context("invalid transparent output")?;
                db.put_received_transparent_utxo(&output)?;
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn put_subtree_roots(
        &self,
        protocol: ShieldedProtocol,
        roots: Vec<SubtreeRootData>,
    ) -> Result<()> {
        self.with_db(move |db| {
            match protocol {
                ShieldedProtocol::Sapling => {
                    db.put_sapling_subtree_roots(0, &to_roots::<sapling::Node>(roots)?)?
                }
                ShieldedProtocol::Orchard => db.put_orchard_subtree_roots(
                    0,
                    &to_roots::<orchard::tree::MerkleHashOrchard>(roots)?,
                )?,
                ShieldedProtocol::Ironwood => db.put_ironwood_subtree_roots(
                    0,
                    &to_roots::<orchard::tree::MerkleHashOrchard>(roots)?,
                )?,
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn update_chain_tip(&self, height: u64) -> Result<()> {
        let height = u32::try_from(height).context("chain height exceeds the wallet range")?;
        self.with_db(move |db| {
            db.update_chain_tip(BlockHeight::from_u32(height))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn suggested_scan_ranges(&self) -> Result<Vec<ScanRange>> {
        self.with_db(|db| db.suggest_scan_ranges().map_err(Into::into))
            .await
    }

    pub(crate) async fn scan_batch(
        &self,
        range: ScanRange,
        blocks: Vec<CompactBlock>,
        chain_state: ChainState,
    ) -> Result<ScanOutcome> {
        self.with_db(move |db| {
            let cache = MemoryBlockCache(
                blocks
                    .into_iter()
                    .map(|block| (block.height as u32, block))
                    .collect(),
            );
            match scan_cached_blocks(
                &regtest_network(),
                &cache,
                db,
                range.block_range().start,
                &chain_state,
                range.len(),
            ) {
                Err(error::Error::Scan(error)) if error.is_continuity_error() => {
                    Ok(ScanOutcome::Rewind(
                        u32::from(error.at_height().saturating_sub(10))
                            .max(WALLET_BIRTHDAY_HEIGHT - 1),
                    ))
                }
                Ok(_) => Ok(ScanOutcome::Scanned(
                    db.suggest_scan_ranges()?
                        .first()
                        .is_some_and(|latest| latest.priority() > range.priority()),
                )),
                Err(error) => Err(anyhow::anyhow!("scanning compact blocks: {error}")),
            }
        })
        .await
    }

    pub(crate) async fn rewind_to_height(&self, chain_state: ChainState) -> Result<()> {
        self.with_db(move |db| {
            db.transactionally_with_extension::<_, _, anyhow::Error>(|wallet, ext| {
                let height = u32::from(chain_state.block_height());
                let hash = chain_state.block_hash().to_string();
                wallet.truncate_to_chain_state(chain_state)?;
                ext.execute(
                    "UPDATE ext_tsz_treasury_cursor SET height=?1,block_hash=?2,updated_at=CURRENT_TIMESTAMP WHERE height>=?1",
                    rusqlite::params![height, hash],
                )?;
                ext.execute(
                    "UPDATE ext_ths_transparent_history SET height=?1,block_hash=?2 WHERE height>=?1",
                    rusqlite::params![height, hash],
                )?;
                Ok(())
            })
        })
        .await
    }

    pub(crate) async fn block_hash(&self, height: u32) -> Result<Option<String>> {
        self.with_db(move |db| {
            Ok(db
                .get_block_hash(height.into())?
                .map(|hash| hash.to_string()))
        })
        .await
    }

    pub(crate) async fn max_scanned_checkpoint(&self) -> Result<Option<ChainCheckpoint>> {
        self.with_db(|db| {
            Ok(db.block_max_scanned()?.map(|block| ChainCheckpoint {
                height: u64::from(u32::from(block.block_height())),
                hash: block.block_hash().to_string(),
            }))
        })
        .await
    }

    pub(crate) async fn scanned_checkpoint(&self) -> Result<Option<ChainCheckpoint>> {
        self.with_db(|db| {
            Ok(db.block_fully_scanned()?.map(|block| ChainCheckpoint {
                height: u64::from(u32::from(block.block_height())),
                hash: block.block_hash().to_string(),
            }))
        })
        .await
    }

    pub(crate) async fn latest_checkpoint(&self) -> Result<ChainCheckpoint> {
        let query = async {
            let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
            let tip = client
                .get_latest_block(ChainSpec::default())
                .await?
                .into_inner();
            let hash = BlockHash::try_from_slice(&tip.hash)
                .context("lightwalletd returned an invalid tip hash")?;
            Ok::<_, anyhow::Error>(ChainCheckpoint {
                height: tip.height,
                hash: hash.to_string(),
            })
        };
        tokio::time::timeout(Duration::from_secs(5), query)
            .await
            .context("reading lightwalletd tip timed out")?
    }

    /// Waits until lightwalletd serves `target` from its block cache. lightwalletd answers
    /// `GetLatestBlock` straight from Zakura, so a height alone does not prove the indexer caught
    /// up: right after a rollback its cache still serves the removed block at that height until
    /// its next poll. `canonical_hash` reads Zakura's hash at a height (`None` once the chain is
    /// shorter), so the wait ends as soon as a reorg replaces `target` or a rollback removes it
    /// instead of waiting for a block that will never be indexed.
    pub async fn wait_for_block<F, Fut>(
        &self,
        target: &ChainCheckpoint,
        timeout: Duration,
        canonical_hash: F,
    ) -> Result<()>
    where
        F: Fn(u32) -> Fut,
        Fut: std::future::Future<Output = Result<Option<String>>>,
    {
        let height = u32::try_from(target.height)?;
        let mut served = None;
        let wait = async {
            let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
            loop {
                let request = BlockId {
                    height: target.height,
                    hash: Vec::new(),
                };
                let indexed = match client.get_block(request).await {
                    Ok(block) => Some(
                        BlockHash::try_from_slice(&block.into_inner().hash)
                            .context("lightwalletd returned an invalid block hash")?
                            .to_string(),
                    ),
                    // Zakura does not have the height yet.
                    Err(status) if status.code() == tonic::Code::OutOfRange => None,
                    Err(status) => return Err(status.into()),
                };
                if indexed.as_deref() == Some(target.hash.as_str()) {
                    return Ok::<_, anyhow::Error>(());
                }
                match canonical_hash(height).await? {
                    Some(canonical) if canonical == target.hash => {}
                    Some(canonical) => anyhow::bail!(
                        "Zakura replaced block {} at height {height} with {canonical} before lightwalletd indexed it",
                        target.hash
                    ),
                    None => anyhow::bail!(
                        "Zakura rolled back below height {height} before lightwalletd indexed block {}",
                        target.hash
                    ),
                }
                served = indexed;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        };
        let error = match tokio::time::timeout(timeout, wait).await {
            Ok(result) => return result,
            Err(error) => error,
        };
        let served = served.unwrap_or_else(|| "none".into());
        Err(error).with_context(|| format!(
            "lightwalletd did not index Zakura block {} at height {height} within {} seconds (block served at that height: {served})",
            target.hash,
            timeout.as_secs()
        ))
    }

    pub async fn apply_balances(&self, accounts: &mut [Account]) -> Result<()> {
        let account_ids = self.account_ids.clone();
        let Some(balances) = self
            .with_db(move |db| {
                db.transactionally_with_extension::<_, _, anyhow::Error>(|wallet, ext| {
                    let Some(scanned) = wallet.block_fully_scanned()? else {
                        return Ok(None);
                    };
                    // SDK totals retain unmined credits and input reservations after a rewind.
                    // Publish canonical holdings without deleting those pending transactions.
                    let balances = account_ids
                        .iter()
                        .map(|id| {
                            ext.query_row(
                                "SELECT
                                    COALESCE(SUM(CASE WHEN r.pool = 0 THEN r.value ELSE 0 END), 0),
                                    COALESCE(SUM(CASE WHEN r.pool = 4 THEN r.value ELSE 0 END), 0)
                                 FROM v_received_outputs r
                                 JOIN accounts a ON a.id = r.account_id
                                 JOIN transactions received ON received.id_tx = r.transaction_id
                                 WHERE a.uuid = ?1 AND received.mined_height <= ?2
                                   AND NOT EXISTS (
                                       SELECT 1 FROM v_received_output_spends s
                                       JOIN transactions spent ON spent.id_tx = s.transaction_id
                                       WHERE s.pool = r.pool
                                         AND s.received_output_id = r.id_within_pool_table
                                         AND spent.mined_height <= ?2
                                   )",
                                rusqlite::params![
                                    id.expose_uuid(),
                                    u32::from(scanned.block_height())
                                ],
                                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
                            )
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(Some(balances))
                })
            })
            .await?
        else {
            return Ok(());
        };
        for (account, (transparent, ironwood)) in accounts.iter_mut().zip(balances) {
            account.transparent_zatoshi = transparent;
            account.ironwood_zatoshi = ironwood;
        }
        Ok(())
    }

    pub(crate) async fn public_transaction_enhancements(&self, limit: usize) -> Result<Vec<TxId>> {
        self.with_db(move |db| {
            Ok(db
                .transaction_enhancement_work()?
                .into_iter()
                .filter_map(|work| match work {
                    TransactionEnhancementWork::Public(request) => Some(request.txid()),
                    TransactionEnhancementWork::Private(_) => None,
                })
                .take(limit)
                .collect())
        })
        .await
    }

    pub(crate) async fn enhance_transaction(
        &self,
        expected_txid: TxId,
        raw: Vec<u8>,
        height: u32,
    ) -> Result<()> {
        self.with_db(move |db| {
            let params = regtest_network();
            let height = BlockHeight::from_u32(height);
            let tx = Transaction::read(&raw[..], BranchId::for_height(&params, height))
                .context("parsing transaction from lightwalletd")?;
            anyhow::ensure!(
                tx.txid() == expected_txid,
                "lightwalletd returned a different transaction than requested"
            );
            decrypt_and_store_transaction(&params, db, &tx, Some(height))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn transaction_enhancement_not_found(&self, txid: TxId) -> Result<()> {
        self.with_db(move |db| {
            db.notify_transaction_enhancement_not_found(txid)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
#[path = "../tests/unit/wallet/treasury_sync_tests.rs"]
mod treasury_sync_tests;

#[cfg(test)]
#[path = "../tests/unit/wallet/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/support/transparent_history.rs"]
pub(crate) mod transparent_history_test_support;

const TREASURY_SHIELDING_JOURNAL_ID: &str = "private-treasury-shielding-v1";
