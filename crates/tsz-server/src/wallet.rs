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
    proto::{
        compact_formats::CompactBlock,
        service::{ChainSpec, RawTransaction, compact_tx_streamer_client::CompactTxStreamerClient},
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

use crate::rpc::ChainCheckpoint;

mod recovery;

type Db = WalletDb<rusqlite::Connection, LocalNetwork, SystemClock, UnwrapErr<SysRng>>;

const PREPARED_PAYMENTS_MIGRATION_ID: Uuid =
    Uuid::from_u128(0x695f93ac_6935_47e8_8f06_017b1d7ec3aa);

struct PreparedPaymentsMigration;

impl schemerz::Migration<Uuid> for PreparedPaymentsMigration {
    fn id(&self) -> Uuid {
        PREPARED_PAYMENTS_MIGRATION_ID
    }

    fn dependencies(&self) -> HashSet<Uuid> {
        migrations::V_0_22_0_RC2.iter().copied().collect()
    }

    fn description(&self) -> &'static str {
        "Stores prepared payments for durable application retries."
    }
}

impl RusqliteMigration for PreparedPaymentsMigration {
    type Error = WalletMigrationError;

    fn up(&self, db: &rusqlite::Transaction<'_>) -> Result<(), Self::Error> {
        db.execute_batch(
            "CREATE TABLE ext_tsz_prepared_payments (
                activity_id TEXT PRIMARY KEY,
                txid TEXT NOT NULL,
                raw_transaction BLOB NOT NULL,
                expiry_height INTEGER NOT NULL
            );",
        )?;
        Ok(())
    }
}

fn prepared_payment(db: &mut Db, activity_id: &str) -> Result<Option<PreparedPayment>> {
    db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
        Ok(ext
            .query_row(
                "SELECT txid,raw_transaction,expiry_height FROM ext_tsz_prepared_payments WHERE activity_id=?1",
                [activity_id],
                |row| {
                    Ok(PreparedPayment {
                        txid: row.get(0)?,
                        raw_transaction: row.get(1)?,
                        expiry_height: row.get(2)?,
                    })
                },
            )
            .optional()?)
    })
}
pub(crate) const WALLET_BIRTHDAY_HEIGHT: u32 = 2;

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
    #[error("memos can only be sent to the ironwood pool; transparent outputs cannot carry a memo")]
    TransparentMemo,
    #[error("wallet requires reconciliation before constructing a payment")]
    ScanRequired,
}

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

pub(crate) enum ScanOutcome {
    Scanned(bool),
    Rewind(u32),
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

pub struct PreparedPayment {
    pub txid: String,
    pub raw_transaction: Vec<u8>,
    pub expiry_height: u64,
}

#[derive(Clone)]
pub struct RealWallet {
    db: Arc<Mutex<Db>>,
    account_ids: Vec<AccountUuid>,
    lightwalletd: String,
}

/// Database capabilities the send proposal paths share; a single bound keeps
/// `propose_send` and `quote` from drifting apart.
trait ProposalDb:
    WalletWrite
    + InputSource<Error = <Self as WalletRead>::Error, NoteRef: Copy + Eq + Ord + std::fmt::Display>
    + WalletRead<
        AccountId = <Self as InputSource>::AccountId,
        Error: std::error::Error + Send + Sync + 'static,
    >
{
}

impl<T> ProposalDb for T
where
    T: WalletWrite
        + InputSource<Error = <T as WalletRead>::Error>
        + WalletRead<AccountId = <T as InputSource>::AccountId>,
    <T as InputSource>::NoteRef: Copy + Eq + Ord + std::fmt::Display,
    <T as WalletRead>::Error: std::error::Error + Send + Sync + 'static,
{
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

/// Every upgrade through NU7 activates at height 1, so the Orchard pool never
/// accepts deposits on this chain and all new shielded value lives in Ironwood.
/// Keep these heights in sync with the Zakura configuration in `main.rs`.
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
        nu6_1: one,
        nu6_2: one,
        nu6_3: one,
        nu7: one,
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
        WalletMigrator::new()
            .with_seed(SecretVec::new(secret.expose_secret().clone()))
            .with_external_migrations(vec![
                Box::new(PreparedPaymentsMigration),
                Box::new(recovery::TreasuryCursorMigration),
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

        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            account_ids,
            lightwalletd: std::env::var("TSZ_LIGHTWALLETD")
                .unwrap_or_else(|_| "http://127.0.0.1:9067".into()),
        })
    }

    pub(crate) fn lightwalletd(&self) -> &str {
        &self.lightwalletd
    }

    async fn with_db<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Db) -> Result<T> + Send + 'static,
    {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || operation(&mut db.blocking_lock()))
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
                    let roots = roots
                        .into_iter()
                        .map(|root| {
                            Ok(CommitmentTreeRoot::from_parts(
                                BlockHeight::from_u32(root.completing_height),
                                sapling::Node::read(&root.root_hash[..])?,
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    db.put_sapling_subtree_roots(0, &roots)?;
                }
                ShieldedProtocol::Orchard | ShieldedProtocol::Ironwood => {
                    let roots = roots
                        .into_iter()
                        .map(|root| {
                            Ok(CommitmentTreeRoot::from_parts(
                                BlockHeight::from_u32(root.completing_height),
                                orchard::tree::MerkleHashOrchard::read(&root.root_hash[..])?,
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    match protocol {
                        ShieldedProtocol::Orchard => db.put_orchard_subtree_roots(0, &roots)?,
                        ShieldedProtocol::Ironwood => db.put_ironwood_subtree_roots(0, &roots)?,
                        ShieldedProtocol::Sapling => unreachable!(),
                    }
                }
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

    pub async fn wait_for_height(&self, target: u64, timeout: Duration) -> Result<()> {
        let mut latest_indexed = None;
        let wait = async {
            let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
            loop {
                let indexed = client
                    .get_latest_block(ChainSpec::default())
                    .await?
                    .into_inner()
                    .height;
                latest_indexed = Some(indexed);
                if indexed >= target {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        };
        if let Ok(result) = tokio::time::timeout(timeout, wait).await {
            return result;
        }
        let latest_indexed =
            latest_indexed.map_or_else(|| "unknown".into(), |height| height.to_string());
        bail!(
            "lightwalletd did not index Zakura height {target} within {} seconds (latest indexed height: {latest_indexed})",
            timeout.as_secs()
        );
    }

    pub async fn apply_balances(&self, accounts: &mut [Account]) -> Result<()> {
        let Some(summary) = self
            .with_db(|db| Ok(db.get_wallet_summary(ConfirmationsPolicy::MIN)?))
            .await?
        else {
            return Ok(());
        };
        for (account, wallet_id) in accounts.iter_mut().zip(&self.account_ids) {
            if let Some(balance) = summary.account_balances().get(wallet_id) {
                account.transparent_zatoshi = u64::from(balance.unshielded_balance().total());
                account.ironwood_zatoshi = u64::from(balance.ironwood_balance().total());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub async fn enhance_transaction(&self, raw_hex: &str, height: u32) -> Result<()> {
        let raw_hex = raw_hex.to_owned();
        self.with_db(move |db| {
            let params = regtest_network();
            let height = BlockHeight::from_u32(height);
            let raw = hex::decode(raw_hex).context("invalid transaction hex from Zakura")?;
            let tx = Transaction::read(&raw[..], BranchId::for_height(&params, height))
                .context("parsing transaction from Zakura")?;
            decrypt_and_store_transaction(&params, db, &tx, Some(height))?;
            Ok(())
        })
        .await
    }

    /// Builds the proposal a send would use, without signing or broadcasting.
    fn propose_send<DbT>(
        db: &mut DbT,
        params: &LocalNetwork,
        account_id: <DbT as InputSource>::AccountId,
        source_pool: &str,
        recipient: &Address,
        amount: Zatoshis,
        memo: Option<MemoBytes>,
    ) -> Result<Proposal<StandardFeeRule, <DbT as InputSource>::NoteRef>>
    where
        DbT: ProposalDb,
    {
        let proposal = if source_pool == "transparent" {
            let request = TransactionRequest::new(vec![Payment::new(
                recipient.to_zcash_address(params),
                Some(amount),
                memo,
                None,
                None,
                vec![],
            )?])?;
            let selector = GreedyInputSelector::<DbT>::new();
            let change = SingleOutputChangeStrategy::<DbT>::new(
                StandardFeeRule::Zip317,
                None,
                ShieldedPool::Ironwood,
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
        } else if source_pool == "ironwood" {
            // Once NU6.3 is active the SDK builds a unified-address payment's
            // Orchard receiver as an Ironwood output, and routes change there too.
            propose_standard_transfer_to_address::<_, _, Infallible>(
                db,
                params,
                StandardFeeRule::Zip317,
                account_id,
                ConfirmationsPolicy::MIN,
                recipient,
                amount,
                memo,
                None,
                ShieldedPool::Ironwood,
                None,
                None,
            )
        } else {
            bail!("source pool must be transparent or ironwood")
        }
        .map_err(|error| match error {
            WalletError::InsufficientFunds {
                available,
                required,
            } => anyhow::Error::new(PaymentError::InsufficientFunds {
                available: u64::from(available),
                required: u64::from(required),
            }),
            WalletError::ScanRequired => anyhow::Error::new(PaymentError::ScanRequired),
            error => anyhow::anyhow!("proposing {source_pool} transaction: {error}"),
        })?;
        Ok(proposal)
    }

    fn send_input(
        &self,
        from_account: u8,
        destination: &str,
    ) -> Result<(u8, AccountUuid, LocalNetwork, Address)> {
        let account_index = from_account
            .checked_sub(1)
            .context("invalid source account")?;
        let account_id = *self
            .account_ids
            .get(account_index as usize)
            .context("source account does not exist")?;
        let params = regtest_network();
        let recipient =
            Address::decode(&params, destination).context("invalid destination address")?;
        Ok((account_index, account_id, params, recipient))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn prepare(
        &self,
        activity_id: Option<&str>,
        seed_hex: &str,
        from_account: u8,
        source_pool: &str,
        destination: &str,
        amount: u64,
        memo: Option<MemoBytes>,
    ) -> Result<PreparedPayment> {
        let mut db = self.db.lock().await;
        if let Some(activity_id) = activity_id
            && let Some(prepared) = prepared_payment(&mut db, activity_id)?
            && (prepared.expiry_height == 0
                || db
                    .chain_height()?
                    .is_none_or(|height| u64::from(u32::from(height)) < prepared.expiry_height))
        {
            return Ok(prepared);
        }
        let (account_index, account_id, params, recipient) =
            self.send_input(from_account, destination)?;
        if memo.is_some() && matches!(recipient, Address::Transparent(_) | Address::Tex(_)) {
            return Err(PaymentError::TransparentMemo.into());
        }
        let amount = Zatoshis::from_u64(amount).map_err(|_| anyhow::anyhow!("invalid amount"))?;
        let proposal = Self::propose_send(
            &mut *db,
            &params,
            account_id,
            source_pool,
            &recipient,
            amount,
            memo,
        )?;
        let seed = hex::decode(seed_hex)?;
        let usk = UnifiedSpendingKey::from_seed(
            &params,
            &seed,
            zip32::AccountId::try_from(u32::from(account_index))
                .map_err(|_| anyhow::anyhow!("invalid account index"))?,
        )
        .map_err(|e| anyhow::anyhow!("deriving spending key: {e:?}"))?;
        db.transactionally_with_extension::<_, _, anyhow::Error>(|wallet, ext| {
            let prover = LocalTxProver::bundled();
            let txids = create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
                wallet,
                &params,
                &prover,
                &prover,
                &SpendingKeys::from_unified_spending_key(usk),
                OvkPolicy::Sender,
                &proposal,
                None,
            )
            .map_err(|error| anyhow::anyhow!("building transaction: {error}"))?;
            let txid = *txids.first();
            let tx = wallet
                .get_transaction(txid)?
                .context("built transaction was not stored")?;
            let mut raw_transaction = vec![];
            tx.write(&mut raw_transaction)?;
            let prepared = PreparedPayment {
                txid: txid.to_string(),
                raw_transaction,
                expiry_height: u64::from(u32::from(tx.expiry_height())),
            };
            if let Some(activity_id) = activity_id {
                ext.execute(
                    "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height)
                     VALUES(?1,?2,?3,?4)
                     ON CONFLICT(activity_id) DO UPDATE SET
                       txid=excluded.txid,
                       raw_transaction=excluded.raw_transaction,
                       expiry_height=excluded.expiry_height",
                    rusqlite::params![
                        activity_id,
                        prepared.txid,
                        prepared.raw_transaction,
                        prepared.expiry_height
                    ],
                )?;
            }
            Ok(prepared)
        })
    }

    pub async fn has_prepared(&self, activity_id: &str) -> Result<bool> {
        let mut db = self.db.lock().await;
        Ok(prepared_payment(&mut db, activity_id)?.is_some())
    }

    pub async fn recover_prepared(&self, txid: &str) -> Result<Option<PreparedPayment>> {
        let txid = TxId::from_hex(txid).context("invalid wallet transaction id")?;
        let db = self.db.lock().await;
        let Some(transaction) = db.get_transaction(txid)? else {
            return Ok(None);
        };
        let mut raw_transaction = vec![];
        transaction.write(&mut raw_transaction)?;
        Ok(Some(PreparedPayment {
            txid: txid.to_string(),
            raw_transaction,
            expiry_height: u64::from(u32::from(transaction.expiry_height())),
        }))
    }

    pub async fn broadcast(&self, raw_transaction: &[u8]) -> Result<()> {
        let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
        let result = client
            .send_transaction(RawTransaction {
                data: raw_transaction.to_vec(),
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
        Ok(())
    }

    /// Returns the exact fee and maximum spendable amount for emptying a pool.
    pub async fn send_quote(
        &self,
        from_account: u8,
        source_pool: &str,
        destination: &str,
    ) -> Result<SendQuote> {
        let (_, account_id, params, recipient) = self.send_input(from_account, destination)?;
        let mut db = self.db.lock().await;
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
        DbT: ProposalDb,
    {
        if source_pool == "ironwood" {
            match propose_send_max_transfer::<_, _, _, Infallible>(
                db,
                params,
                account_id,
                &[ShieldedPool::Ironwood],
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
                None,
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
            bail!("source pool must be transparent or ironwood")
        }
    }

    pub async fn shield_coinbase(
        &self,
        seed_hex: &str,
        treasury_account: u8,
        from: &str,
        to: &str,
        minimum_net: u64,
    ) -> Result<String> {
        if treasury_account != crate::db::TREASURY_ACCOUNT_ID {
            bail!("coinbase shielding requires the treasury account");
        }
        let seed_hex = seed_hex.to_owned();
        let from = from.to_owned();
        let to = to.to_owned();
        let (txid, raw) = self
            .with_db(move |db| {
                let params = regtest_network();
                let from =
                    match Address::decode(&params, &from).context("invalid treasury address")? {
                        Address::Transparent(address) => address,
                        _ => bail!("treasury address is not transparent"),
                    };
                let to = Address::decode(&params, &to)
                    .context("invalid shielding address")?
                    .to_zcash_address(&params);
                let threshold = Zatoshis::from_u64(minimum_net)
                    .map_err(|_| anyhow::anyhow!("invalid shielding threshold"))?;
                // Existing wallets may already hold thousands of rewards. Grow the SDK's
                // input cap only until its fee-aware proposal covers this payment.
                let mut limit = Some(1);
                let proposal = loop {
                    match propose_shielding_coinbase::<_, _, _, _, Infallible>(
                        &mut *db,
                        &params,
                        &GreedyInputSelector::new(),
                        &StandardFeeRule::Zip317,
                        threshold,
                        &[from],
                        to.clone(),
                        None,
                        limit,
                        None,
                    ) {
                        Ok(proposal) => break proposal,
                        Err(WalletError::InsufficientFunds { .. }) if limit.is_some() => {
                            limit = limit.filter(|value| *value < 2048).map(|value| value * 2);
                        }
                        Err(WalletError::InsufficientFunds { .. } | WalletError::ScanRequired) => {
                            return Err(anyhow::Error::new(PaymentError::TreasuryExhausted));
                        }
                        Err(error) => {
                            return Err(anyhow::anyhow!("proposing coinbase shielding: {error}"));
                        }
                    }
                };
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
                Ok((txid.to_string(), raw))
            })
            .await?;
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
        Ok(txid)
    }
}

#[cfg(test)]
mod treasury_sync_tests {
    use super::*;
    use crate::db::{Store, TREASURY_ACCOUNT_ID, USER_ACCOUNT_COUNT};

    #[tokio::test]
    async fn wallet_migration_installs_only_the_treasury_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("server.db")).unwrap();
        store.initialize().unwrap();
        let seed = SecretVec::new(hex::decode(store.seed().unwrap()).unwrap());
        let mut db = WalletDb::for_path(
            dir.path().join("wallet.db"),
            regtest_network(),
            SystemClock,
            UnwrapErr(SysRng),
        )
        .unwrap();
        zcash_client_sqlite::wallet::init::init_wallet_db(
            &mut db,
            Some(SecretVec::new(seed.expose_secret().clone())),
        )
        .unwrap();
        let birthday =
            AccountBirthday::from_parts(ChainState::empty(1.into(), BlockHash([0; 32])), None);
        for id in 1..=USER_ACCOUNT_COUNT {
            db.create_account(&format!("Account {id}"), &seed, &birthday, None)
                .unwrap();
        }
        drop(db);

        let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        wallet
            .with_db(|db| {
                assert_eq!(db.get_account_ids()?.len(), 6);
                let (cursor, prepared): (u32, u32) =
                    db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
                        Ok((
                            ext.query_row(
                                "SELECT COUNT(*) FROM sqlite_master WHERE name='ext_tsz_treasury_cursor'",
                                [],
                                |row| row.get(0),
                            )?,
                            ext.query_row(
                                "SELECT COUNT(*) FROM sqlite_master WHERE name='ext_tsz_prepared_transactions'",
                                [],
                                |row| row.get(0),
                            )?,
                        ))
                    })?;
                assert_eq!((cursor, prepared), (1, 0));
                Ok(())
            })
            .await
            .unwrap();
        drop(wallet);
        let reopened = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        assert!(reopened.treasury_cursor().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn routine_transparent_queries_exclude_the_treasury() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("server.db")).unwrap();
        store.initialize().unwrap();
        let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();

        let queries = wallet.public_transparent_queries().await.unwrap();
        assert_eq!(queries.len(), usize::from(USER_ACCOUNT_COUNT));
        for id in 1..=USER_ACCOUNT_COUNT {
            assert!(queries.iter().any(|query| {
                query
                    .addresses
                    .contains(&store.account(id).unwrap().transparent_address)
            }));
        }
        assert!(!queries.iter().any(|query| {
            query.addresses.contains(
                &store
                    .account(TREASURY_ACCOUNT_ID)
                    .unwrap()
                    .transparent_address,
            )
        }));
    }

    #[tokio::test]
    async fn height_wait_deadline_includes_stalled_lightwalletd_calls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("server.db")).unwrap();
        store.initialize().unwrap();
        let mut wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        wallet.lightwalletd = format!("http://{}", listener.local_addr().unwrap());

        let result = tokio::time::timeout(
            Duration::from_millis(250),
            wallet.wait_for_height(1, Duration::from_millis(50)),
        )
        .await;

        assert!(result.is_ok(), "height wait ignored its deadline");
        assert!(result.unwrap().is_err());
    }

    #[tokio::test]
    async fn database_work_does_not_block_the_async_executor() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("server.db")).unwrap();
        store.initialize().unwrap();
        let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        let work = tokio::spawn(async move {
            wallet
                .with_db(move |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    Ok(())
                })
                .await
        });
        entered_rx.await.unwrap();
        let elapsed = started.elapsed();
        release_tx.send(()).unwrap();
        work.await.unwrap().unwrap();
        assert!(
            elapsed < Duration::from_millis(750),
            "database work blocked the async executor for {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod prepared_tests {
    use transparent::{
        bundle::{OutPoint, TxOut},
        keys::TransparentKeyScope,
    };
    use zcash_client_backend::{
        data_api::testing::{
            self, AddressType, IronwoodFvk, TestBuilder, orchard::OrchardPoolTester,
            pool::ShieldedPoolTester,
        },
        wallet::WalletTransparentOutput,
    };
    use zcash_client_sqlite::testing::{
        BlockCache,
        db::{TestDb, TestDbFactory},
    };
    use zcash_keys::keys::UnifiedAddressRequest;
    use zcash_primitives::block::BlockHash;

    use super::*;

    type TestState = testing::TestState<BlockCache, TestDb, LocalNetwork>;

    /// The SDK's default test network with every upgrade through NU7 active from
    /// Sapling activation, like the local chain, so shielded value lives in Ironwood.
    fn test_state() -> TestState {
        let activation = Some(BlockHeight::from_u32(100_000));
        let network = LocalNetwork {
            nu6: activation,
            nu6_1: activation,
            nu6_2: activation,
            nu6_3: activation,
            nu7: activation,
            ..TestBuilder::<(), ()>::DEFAULT_NETWORK
        };
        TestBuilder::new()
            .with_network(network)
            .with_data_store_factory(TestDbFactory::default())
            .with_block_cache(BlockCache::new())
            .with_account_from_sapling_activation(BlockHash([0; 32]))
            .build()
    }

    fn add_empty_blocks(st: &mut TestState, count: usize) {
        for _ in 0..count {
            let (height, _) = st.generate_empty_block();
            st.scan_cached_blocks(height, 1);
        }
    }

    /// Receives one Ironwood (version 3) note of `value`, then advances the chain so
    /// the note's shard is scanned and an anchor is available to spend it.
    fn fund_ironwood(st: &mut TestState, value: u64) {
        let account_id = st.test_account().unwrap().id();
        let fvk = IronwoodFvk(OrchardPoolTester::test_account_fvk(st));
        let value = Zatoshis::const_from_u64(value);
        let (height, _, _) = st.generate_next_block(&fvk, AddressType::DefaultExternal, value);
        st.scan_cached_blocks(height, 1);
        assert_eq!(st.get_total_balance(account_id), value);
        add_empty_blocks(st, 5);
    }

    fn recipient(st: &TestState, pool: &str) -> Address {
        let account_id = st.test_account().unwrap().id();
        let ua = st
            .wallet()
            .get_last_generated_address_matching(
                account_id,
                UnifiedAddressRequest::AllAvailableKeys,
            )
            .unwrap()
            .unwrap();
        if pool == "transparent" {
            Address::Transparent(*ua.transparent().unwrap())
        } else {
            Address::Unified(ua)
        }
    }

    fn quote(st: &mut TestState, source_pool: &str, destination_pool: &str) -> Result<SendQuote> {
        let recipient = recipient(st, destination_pool);
        let account_id = st.test_account().unwrap().id();
        let params = *st.network();
        RealWallet::quote(
            st.wallet_mut(),
            &params,
            account_id,
            source_pool,
            &recipient,
        )
    }

    fn fund_transparent(st: &mut TestState, value: u64) {
        add_empty_blocks(st, 10);
        let account_id = st.test_account().unwrap().id();
        let taddr = match recipient(st, "transparent") {
            Address::Transparent(address) => address,
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
    fn ironwood_quote_reports_fee_and_max() {
        let mut st = test_state();
        fund_ironwood(&mut st, 100_000_000);
        let quote = quote(&mut st, "ironwood", "ironwood").unwrap();
        assert_eq!(quote.available_zatoshi, 100_000_000);
        assert_eq!(quote.fee_zatoshi, 10_000);
        assert_eq!(quote.max_zatoshi, 99_990_000);
    }

    #[test]
    fn ironwood_quote_max_is_zero_when_balance_only_covers_the_fee() {
        let mut st = test_state();
        fund_ironwood(&mut st, 10_000);
        let quote = quote(&mut st, "ironwood", "ironwood").unwrap();
        assert_eq!(quote.available_zatoshi, 10_000);
        assert_eq!(quote.fee_zatoshi, 10_000);
        assert_eq!(quote.max_zatoshi, 0);
    }

    #[test]
    fn ironwood_quote_max_is_zero_when_balance_is_below_the_fee() {
        let mut st = test_state();
        fund_ironwood(&mut st, 9_000);
        let quote = quote(&mut st, "ironwood", "ironwood").unwrap();
        assert_eq!(quote.available_zatoshi, 9_000);
        assert_eq!(quote.fee_zatoshi, 10_000);
        assert_eq!(quote.max_zatoshi, 0);
    }

    #[test]
    fn ironwood_quote_reports_zero_max_for_an_empty_account() {
        let mut st = test_state();
        add_empty_blocks(&mut st, 10);
        let quote = quote(&mut st, "ironwood", "ironwood").unwrap();
        assert_eq!(quote.available_zatoshi, 0);
        assert_eq!(quote.fee_zatoshi, 10_000);
        assert_eq!(quote.max_zatoshi, 0);
    }

    // ZIP-317: max(⌈148/150⌉, 0) transparent + 2 Ironwood actions (recipient,
    // change) = 3 logical actions, above the 2-action grace → 3 × 5_000.
    #[test]
    fn transparent_quote_reports_fee_and_max() {
        let mut st = test_state();
        fund_transparent(&mut st, 100_000_000);
        let quote = quote(&mut st, "transparent", "ironwood").unwrap();
        assert_eq!(quote.available_zatoshi, 100_000_000);
        assert_eq!(quote.fee_zatoshi, 15_000);
        assert_eq!(quote.max_zatoshi, 99_985_000);
    }

    // ZIP-317: max(⌈148/150⌉, ⌈34/34⌉) transparent + 1 Ironwood change action
    // = 2 logical actions, inside the 2-action grace → 2 × 5_000.
    #[test]
    fn transparent_quote_to_transparent_reports_fee_and_max() {
        let mut st = test_state();
        fund_transparent(&mut st, 100_000_000);
        let quote = quote(&mut st, "transparent", "transparent").unwrap();
        assert_eq!(quote.available_zatoshi, 100_000_000);
        assert_eq!(quote.fee_zatoshi, 10_000);
        assert_eq!(quote.max_zatoshi, 99_990_000);
    }

    #[test]
    fn transparent_quote_max_is_zero_when_balance_is_below_the_fee() {
        let mut st = test_state();
        fund_transparent(&mut st, 9_000);
        let quote = quote(&mut st, "transparent", "ironwood").unwrap();
        assert_eq!(quote.available_zatoshi, 9_000);
        assert_eq!(quote.fee_zatoshi, 15_000);
        assert_eq!(quote.max_zatoshi, 0);
    }

    #[test]
    fn transparent_quote_reports_zero_max_for_dust() {
        let mut st = test_state();
        fund_transparent(&mut st, 5_000);
        let quote = quote(&mut st, "transparent", "ironwood").unwrap();
        assert_eq!(quote.available_zatoshi, 0);
        // No spendable input, so only the two Ironwood actions pay: the
        // 2-action grace floor still applies once an input exists.
        assert_eq!(quote.fee_zatoshi, 10_000);
        assert_eq!(quote.max_zatoshi, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prepared_payment_journal_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let seed = hex::encode([7_u8; 64]);
        let wallet = RealWallet::open(dir.path(), &seed).unwrap();
        drop(wallet);

        let db = rusqlite::Connection::open(dir.path().join("wallet.db")).unwrap();
        db.execute(
            "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height) VALUES(?1,?2,?3,?4)",
            rusqlite::params!["activity-1", "txid-1", b"signed transaction", 140_u64],
        )
        .unwrap();
        drop(db);

        let wallet = RealWallet::open(dir.path(), &seed).unwrap();
        assert!(wallet.has_prepared("activity-1").await.unwrap());
        assert!(!wallet.has_prepared("missing").await.unwrap());
        let recovered = wallet
            .prepare(
                Some("activity-1"),
                "invalid seed",
                0,
                "invalid pool",
                "invalid address",
                0,
                None,
            )
            .await
            .unwrap();
        assert_eq!(recovered.txid, "txid-1");
        assert_eq!(recovered.raw_transaction, b"signed transaction");
    }
}
