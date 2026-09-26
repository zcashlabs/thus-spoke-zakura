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
use tonic::transport::{Channel, Endpoint};
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

use crate::db::Account;

type Db = WalletDb<rusqlite::Connection, LocalNetwork, SystemClock, UnwrapErr<SysRng>>;

const LIGHTWALLETD_TIMEOUT: Duration = Duration::from_secs(30);
// lightwalletd's grpc server drops clients that ping more often than every
// 5 minutes, so a stalled stream is detected after this plus the timeout.
const LIGHTWALLETD_KEEPALIVE: Duration = Duration::from_secs(6 * 60);

/// times out each request, and uses keepalive pings to drop a connection that
/// stops responding, so a stalled block stream fails too.
async fn lightwalletd_client(
    endpoint: &str,
    timeout: Duration,
    keepalive: Duration,
) -> Result<CompactTxStreamerClient<Channel>> {
    let channel = Endpoint::from_shared(endpoint.to_owned())?
        .connect_timeout(timeout)
        .timeout(timeout)
        .http2_keep_alive_interval(keepalive)
        .keep_alive_timeout(timeout)
        .connect()
        .await?;
    Ok(CompactTxStreamerClient::new(channel))
}

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

    async fn client(&self) -> Result<CompactTxStreamerClient<Channel>> {
        lightwalletd_client(
            &self.lightwalletd,
            LIGHTWALLETD_TIMEOUT,
            LIGHTWALLETD_KEEPALIVE,
        )
        .await
    }

    pub async fn sync(&self) -> Result<()> {
        let cache = MemoryBlockCache::default();
        let mut client = self.client().await?;
        let mut db = self.db.lock().await;
        sync::run(&mut client, &regtest_network(), &cache, &mut *db, 100)
            .await
            .map_err(|e| anyhow::anyhow!("wallet sync failed: {e}"))
    }

    pub async fn wait_for_height(&self, target: u64, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut client = self.client().await?;
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
        let mut client = self.client().await?;
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
        let mut client = self.client().await?;
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
        let mut client = self.client().await?;
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
    use super::*;
    use zcash_client_backend::proto::service::{BlockId, BlockRange};

    const LIMIT: Duration = Duration::from_millis(100);

    /// a fake lightwalletd that takes one request and never finishes it. with
    /// `go_silent` it sends response headers and then stops reading the
    /// connection, so keepalive pings go unanswered as well.
    async fn stuck_lightwalletd(go_silent: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut connection = h2::server::handshake(socket).await.unwrap();
            let (_request, mut respond) = connection.accept().await.unwrap().unwrap();
            if go_silent {
                let headers = http::Response::builder()
                    .header("content-type", "application/grpc")
                    .body(())
                    .unwrap();
                let _stream = respond.send_response(headers, false).unwrap();
                let _ = tokio::time::timeout(Duration::from_millis(20), connection.accept()).await;
                std::future::pending::<()>().await;
            }
            while connection.accept().await.is_some() {}
        });
        endpoint
    }

    async fn fails_in_time(call: impl Future<Output = Result<()>>) {
        let result = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("lightwalletd call hung instead of timing out");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn gives_up_on_a_request_that_is_never_answered() {
        let endpoint = stuck_lightwalletd(false).await;
        fails_in_time(async {
            lightwalletd_client(&endpoint, LIMIT, LIMIT)
                .await?
                .get_latest_block(ChainSpec::default())
                .await?;
            Ok(())
        })
        .await;
    }

    #[tokio::test]
    async fn gives_up_on_a_block_stream_that_stalls() {
        let endpoint = stuck_lightwalletd(true).await;
        fails_in_time(async {
            let block = |height| BlockId {
                height,
                hash: vec![],
            };
            lightwalletd_client(&endpoint, LIMIT, LIMIT)
                .await?
                .get_block_range(BlockRange {
                    start: Some(block(1)),
                    end: Some(block(2)),
                    ..Default::default()
                })
                .await?
                .into_inner()
                .message()
                .await?;
            Ok(())
        })
        .await;
    }
}
