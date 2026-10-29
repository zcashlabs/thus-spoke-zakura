use super::{RealWallet, journal::prepared_payment_txids};
use crate::db::{TREASURY_ACCOUNT_ID, USER_ACCOUNT_COUNT};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Serialize, Serializer};
use std::collections::{BTreeSet, HashMap};
use uuid::Uuid;
use zcash_client_backend::data_api::{
    WalletRead,
    enhance_pir::{
        EnhancePirRead, EnhancePirSuspension, EnhancePirWork, TransactionEnhancementWork,
    },
};
use zcash_protocol::{
    TxId,
    memo::{Memo, MemoBytes},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CanonicalActivity {
    pub(crate) direction: &'static str,
    pub(crate) from_account: Option<u8>,
    pub(crate) to_account: Option<u8>,
    pub(crate) from_accounts: Vec<u8>,
    pub(crate) to_accounts: Vec<u8>,
    pub(crate) recipient_addresses: Vec<String>,
    pub(crate) external_addresses: Vec<String>,
    pub(crate) source_pools: Vec<String>,
    pub(crate) destination_pools: Vec<String>,
    pub(crate) amount_zatoshi: u64,
    pub(crate) received_zatoshi: u64,
    pub(crate) spent_zatoshi: u64,
    pub(crate) net_zatoshi: i64,
    pub(crate) txid: String,
    pub(crate) block_hash: Option<String>,
    pub(crate) mined_height: Option<u32>,
    pub(crate) tx_index: Option<u32>,
    pub(crate) confirmations: u32,
    pub(crate) created_at: Option<String>,
    pub(crate) memos: Vec<String>,
}

#[derive(Clone)]
struct Output {
    pool: Option<String>,
    from_account: Option<u8>,
    to_account: Option<u8>,
    to_treasury: bool,
    address: Option<String>,
    is_change: bool,
    value: u64,
    memo: Option<String>,
}

fn pool_name(code: i64) -> Option<String> {
    match code {
        0 => Some("transparent".to_owned()),
        4 => Some("ironwood".to_owned()),
        _ => None,
    }
}

/// A memo as the wallet stored it: UTF-8 text, or the stored bytes as hex.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StoredMemo {
    Text(String),
    Hex(String),
}

/// Decodes the SDK's unpadded memo bytes. An empty memo (`0xF6`) and a
/// transparent output's missing memo are both `None`.
fn stored_memo(bytes: Option<Vec<u8>>) -> Option<StoredMemo> {
    let bytes = bytes?;
    let memo = MemoBytes::from_bytes(&bytes)
        .ok()
        .and_then(|memo| Memo::try_from(memo).ok());
    match memo {
        Some(Memo::Empty) => None,
        Some(Memo::Text(text)) => Some(StoredMemo::Text(String::from(&*text))),
        Some(Memo::Arbitrary(_) | Memo::Future(_)) | None => {
            Some(StoredMemo::Hex(hex::encode(&bytes)))
        }
    }
}

fn text_memo(bytes: Option<Vec<u8>>) -> Option<String> {
    match stored_memo(bytes)? {
        StoredMemo::Text(text) => Some(text),
        StoredMemo::Hex(_) => None,
    }
}

fn account_number(account_ids: &[Uuid], uuid: Option<Uuid>) -> Option<u8> {
    uuid.and_then(|uuid| {
        account_ids
            .iter()
            .position(|candidate| *candidate == uuid)
            .and_then(|index| u8::try_from(index + 1).ok())
    })
}

fn user_account(account_ids: &[Uuid], uuid: Option<Uuid>) -> Option<u8> {
    account_number(account_ids, uuid).filter(|id| *id <= USER_ACCOUNT_COUNT)
}

fn unique<T: Ord>(values: impl IntoIterator<Item = T>) -> Vec<T> {
    values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn only<T: Copy>(values: &[T]) -> Option<T> {
    match values {
        [value] => Some(*value),
        _ => None,
    }
}

struct OutputSummary {
    direction: &'static str,
    from_account: Option<u8>,
    to_account: Option<u8>,
    from_accounts: Vec<u8>,
    to_accounts: Vec<u8>,
    recipient_addresses: Vec<String>,
    external_addresses: Vec<String>,
    destination_pools: Vec<String>,
    amount_zatoshi: u64,
    memos: Vec<String>,
}

/// A payment back to its own sender is `self` only when nothing else leaves
/// the development accounts; a batch that also pays an external address is
/// `outgoing`.
pub(crate) fn direction(
    from_accounts: &[u8],
    to_accounts: &[u8],
    pays_external: bool,
) -> &'static str {
    match (from_accounts, to_accounts) {
        ([from], [to]) if from == to => {
            if pays_external {
                "outgoing"
            } else {
                "self"
            }
        }
        ([], []) => "unknown",
        ([], _) => "incoming",
        (_, []) => "outgoing",
        (_, _) => "internal",
    }
}

fn summarize_outputs(outputs: &[Output]) -> Result<OutputSummary> {
    let relevant = outputs
        .iter()
        .filter(|output| {
            !output.to_treasury
                && (!output.is_change
                    || matches!(
                        (output.from_account, output.to_account),
                        (Some(from), Some(to)) if from != to
                    ))
        })
        .collect::<Vec<_>>();
    let from_accounts = unique(relevant.iter().filter_map(|output| output.from_account));
    let to_accounts = unique(relevant.iter().filter_map(|output| output.to_account));
    let amount_zatoshi = relevant.iter().try_fold(0u64, |total, output| {
        total
            .checked_add(output.value)
            .context("wallet transaction output value overflowed")
    })?;
    let external_addresses = unique(
        relevant
            .iter()
            .filter(|output| output.to_account.is_none())
            .filter_map(|output| output.address.clone()),
    );
    Ok(OutputSummary {
        direction: direction(&from_accounts, &to_accounts, !external_addresses.is_empty()),
        from_account: only(&from_accounts),
        to_account: only(&to_accounts),
        from_accounts,
        to_accounts,
        recipient_addresses: unique(relevant.iter().filter_map(|output| output.address.clone())),
        external_addresses,
        destination_pools: unique(relevant.iter().filter_map(|output| output.pool.clone())),
        amount_zatoshi,
        memos: unique(relevant.iter().filter_map(|output| output.memo.clone())),
    })
}

fn outputs_for(db: &Connection, txid: &[u8; 32], account_ids: &[Uuid]) -> Result<Vec<Output>> {
    let mut query = db.prepare(
        "SELECT output_pool,from_account_uuid,to_account_uuid,to_address,is_change,value,memo
         FROM v_tx_outputs WHERE txid=?1 ORDER BY output_pool,output_index",
    )?;
    Ok(query
        .query_map([txid], |row| {
            let from_uuid = row.get(1)?;
            let to_uuid = row.get(2)?;
            Ok(Output {
                pool: pool_name(row.get(0)?),
                from_account: user_account(account_ids, from_uuid),
                to_account: user_account(account_ids, to_uuid),
                to_treasury: account_number(account_ids, to_uuid) == Some(TREASURY_ACCOUNT_ID),
                address: row.get(3)?,
                is_change: row.get(4)?,
                value: row.get(5)?,
                memo: text_memo(row.get(6)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn source_pools_for(db: &Connection, txid: &[u8; 32], account_ids: &[Uuid]) -> Result<Vec<String>> {
    let mut query = db.prepare(
        "SELECT DISTINCT ro.pool
         FROM v_received_output_spends ros
         JOIN v_received_outputs ro
           ON ro.pool=ros.pool AND ro.id_within_pool_table=ros.received_output_id
         JOIN transactions t ON t.id_tx=ros.transaction_id
         JOIN accounts a ON a.id=ros.account_id
         WHERE t.txid=?1 AND a.uuid IN (?2,?3,?4,?5,?6) ORDER BY ro.pool",
    )?;
    let pools = query
        .query_map(
            params![
                txid,
                account_ids[0],
                account_ids[1],
                account_ids[2],
                account_ids[3],
                account_ids[4],
            ],
            |row| row.get::<_, i64>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(pools.into_iter().filter_map(pool_name).collect())
}

fn value_effects_for(
    db: &Connection,
    txid: &[u8; 32],
    account_ids: &[Uuid],
) -> Result<(u64, u64, i64)> {
    let (received, spent, net): (i64, i64, i64) = db.query_row(
        "SELECT COALESCE(SUM(total_received),0),
                COALESCE(SUM(total_spent),0),
                COALESCE(SUM(account_balance_delta),0)
         FROM v_transactions
         WHERE txid=?1 AND account_uuid IN (?2,?3,?4,?5,?6)",
        params![
            txid,
            account_ids[0],
            account_ids[1],
            account_ids[2],
            account_ids[3],
            account_ids[4],
        ],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok((
        u64::try_from(received).context("wallet received value was negative")?,
        u64::try_from(spent).context("wallet spent value was negative")?,
        net,
    ))
}

fn read_canonical_activity(
    db: &Connection,
    account_ids: &[Uuid],
    limit: u32,
) -> Result<Vec<CanonicalActivity>> {
    let tip: Option<u32> = db.query_row("SELECT MAX(height) FROM blocks", [], |row| row.get(0))?;
    let mut query = db.prepare(
        "SELECT DISTINCT vt.txid,vt.mined_height,vt.tx_index,
                b.hash,t.created
         FROM v_transactions vt
         JOIN transactions t ON t.txid=vt.txid
         LEFT JOIN blocks b ON b.height=vt.mined_height
         WHERE account_uuid IN (?1,?2,?3,?4,?5)
         ORDER BY vt.mined_height DESC,vt.tx_index DESC LIMIT ?6",
    )?;
    let mut rows = query.query(params![
        account_ids[0],
        account_ids[1],
        account_ids[2],
        account_ids[3],
        account_ids[4],
        limit.min(100),
    ])?;
    let mut summaries = Vec::new();
    while let Some(row) = rows.next()? {
        let txid_bytes: [u8; 32] = row.get(0)?;
        let mined_height: Option<u32> = row.get(1)?;
        let tx_index: Option<u32> = row.get(2)?;
        let block_hash = row
            .get::<_, Option<[u8; 32]>>(3)?
            .map(|hash| zcash_primitives::block::BlockHash(hash).to_string());
        // `transactions.created` is only available when this wallet created
        // the transaction. An incoming shielded payment does not reveal the
        // sender's creation time; the Regtest block clock is not a substitute.
        let created_at: Option<String> = row.get(4)?;
        let summary = summarize_outputs(&outputs_for(db, &txid_bytes, account_ids)?)?;
        let source_pools = source_pools_for(db, &txid_bytes, account_ids)?;
        let (received_zatoshi, spent_zatoshi, net_zatoshi) =
            value_effects_for(db, &txid_bytes, account_ids)?;
        let confirmations = mined_height
            .zip(tip)
            .map(|(height, tip)| tip.saturating_sub(height).saturating_add(1))
            .unwrap_or(0);
        summaries.push(CanonicalActivity {
            direction: summary.direction,
            from_account: summary.from_account,
            to_account: summary.to_account,
            from_accounts: summary.from_accounts,
            to_accounts: summary.to_accounts,
            recipient_addresses: summary.recipient_addresses,
            external_addresses: summary.external_addresses,
            source_pools,
            destination_pools: summary.destination_pools,
            amount_zatoshi: summary.amount_zatoshi,
            received_zatoshi,
            spent_zatoshi,
            net_zatoshi,
            txid: TxId::from_bytes(txid_bytes).to_string(),
            block_hash,
            mined_height,
            tx_index,
            confirmations,
            created_at,
            memos: summary.memos,
        });
    }
    Ok(summaries)
}

/// Who sent or received one output, as the wallet transaction view reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Party {
    Account(u8),
    Faucet,
}

impl Serialize for Party {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Account(account) => serializer.serialize_u8(*account),
            Self::Faucet => serializer.serialize_str("faucet"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct WalletOutput {
    pub(crate) pool: String,
    pub(crate) index: u32,
    pub(crate) value_zatoshi: u64,
    pub(crate) from: Option<Party>,
    pub(crate) to: Option<Party>,
    pub(crate) to_address: Option<String>,
    pub(crate) change: bool,
    pub(crate) memo: Option<StoredMemo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct WalletSpend {
    pub(crate) account: u8,
    pub(crate) pool: String,
    pub(crate) value_zatoshi: u64,
}

/// Whether the wallet has the full transaction, so memos and change flags are final.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TransactionDetails {
    Pending,
    Complete,
    Unavailable,
}

/// One transaction as the local wallet recorded it for development accounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WalletTransaction {
    pub(crate) fee_zatoshi: Option<u64>,
    pub(crate) details: TransactionDetails,
    pub(crate) outputs: Vec<WalletOutput>,
    pub(crate) spends: Vec<WalletSpend>,
}

impl WalletTransaction {
    /// Keeps only the outputs and spends that `account` sent or received.
    pub(crate) fn for_account(mut self, account: u8) -> Self {
        let party = Some(Party::Account(account));
        self.outputs
            .retain(|output| output.from == party || output.to == party);
        self.spends.retain(|spend| spend.account == account);
        self
    }
}

fn output_pool_name(code: i64) -> Result<String> {
    Ok(match code {
        0 => "transparent",
        2 => "sapling",
        3 => "orchard",
        4 => "ironwood",
        _ => anyhow::bail!("wallet recorded an output in unknown pool {code}"),
    }
    .to_owned())
}

fn party(account_ids: &[Uuid], uuid: Option<Uuid>) -> Option<Party> {
    match account_number(account_ids, uuid)? {
        TREASURY_ACCOUNT_ID => Some(Party::Faucet),
        account if account <= USER_ACCOUNT_COUNT => Some(Party::Account(account)),
        _ => None,
    }
}

fn is_user(party: Option<Party>) -> bool {
    matches!(party, Some(Party::Account(_)))
}

fn wallet_outputs(
    db: &Connection,
    txid: &[u8; 32],
    account_ids: &[Uuid],
) -> Result<Vec<WalletOutput>> {
    let mut query = db.prepare(
        "SELECT output_pool,output_index,from_account_uuid,to_account_uuid,to_address,
                is_change,value,memo
         FROM v_tx_outputs WHERE txid=?1 ORDER BY output_pool,output_index",
    )?;
    let mut rows = query.query([txid])?;
    let mut outputs = Vec::new();
    while let Some(row) = rows.next()? {
        let from = party(account_ids, row.get(2)?);
        let to_address: Option<String> = row.get(4)?;
        // The SDK links a payment to the receiving account only once it scans
        // the block, so until then a payment to another local account names
        // only the address it was sent to.
        let to_uuid = match (row.get::<_, Option<Uuid>>(3)?, &to_address) {
            (None, Some(address)) => address_owner(db, address)?,
            (to_uuid, _) => to_uuid,
        };
        let to = party(account_ids, to_uuid);
        // Only outputs a development account sent or received are reported.
        // The faucet's own change and its payments to other addresses are not.
        if !is_user(from) && !is_user(to) {
            continue;
        }
        let is_change: bool = row.get(5)?;
        outputs.push(WalletOutput {
            pool: output_pool_name(row.get(0)?)?,
            index: row.get(1)?,
            value_zatoshi: row.get(6)?,
            from,
            // A development account paying the faucet's address is a payment
            // to an address outside the development accounts.
            to: to.filter(|party| *party != Party::Faucet),
            to_address,
            // The same rule as `summarize_outputs`: wallet change paid to a
            // different account is a transfer.
            change: is_change && is_user(from) && from == to,
            memo: stored_memo(row.get(7)?),
        });
    }
    Ok(outputs)
}

/// The wallet account that generated `address`, if any.
fn address_owner(db: &Connection, address: &str) -> Result<Option<Uuid>> {
    Ok(db
        .query_row(
            "SELECT ac.uuid FROM addresses a JOIN accounts ac ON ac.id=a.account_id
             WHERE a.address=?1 OR a.cached_transparent_receiver_address=?1 LIMIT 1",
            [address],
            |row| row.get(0),
        )
        .optional()?)
}

fn wallet_spends(
    db: &Connection,
    txid: &[u8; 32],
    account_ids: &[Uuid],
) -> Result<Vec<WalletSpend>> {
    let mut query = db.prepare(
        "SELECT ro.pool,a.uuid,ro.value
         FROM v_received_output_spends ros
         JOIN v_received_outputs ro
           ON ro.pool=ros.pool AND ro.id_within_pool_table=ros.received_output_id
         JOIN transactions t ON t.id_tx=ros.transaction_id
         JOIN accounts a ON a.id=ros.account_id
         WHERE t.txid=?1 ORDER BY ro.pool,ro.id_within_pool_table",
    )?;
    let mut rows = query.query([txid])?;
    let mut spends = Vec::new();
    while let Some(row) = rows.next()? {
        let Some(account) = user_account(account_ids, row.get(1)?) else {
            continue;
        };
        spends.push(WalletSpend {
            account,
            pool: output_pool_name(row.get(0)?)?,
            value_zatoshi: row.get(2)?,
        });
    }
    Ok(spends)
}

/// Whether the wallet recorded the transaction for a development account, and
/// the fee it stored then. The fee is never computed here.
fn user_record(
    db: &Connection,
    txid: &[u8; 32],
    account_ids: &[Uuid],
) -> Result<(bool, Option<u64>)> {
    let mut query = db.prepare("SELECT account_uuid,fee_paid FROM v_transactions WHERE txid=?1")?;
    let mut rows = query.query([txid])?;
    let (mut recorded, mut fee) = (false, None);
    while let Some(row) = rows.next()? {
        if user_account(account_ids, row.get(0)?).is_some() {
            recorded = true;
            fee = fee.max(row.get::<_, Option<u64>>(1)?);
        }
    }
    Ok((recorded, fee))
}

/// Every transaction the wallet still has enhancement work for, through the
/// public or the private route.
fn enhancement_txids(work: Vec<TransactionEnhancementWork>) -> BTreeSet<TxId> {
    work.into_iter()
        .filter_map(|work| match work {
            TransactionEnhancementWork::Public(request) => Some(request.txid()),
            TransactionEnhancementWork::Private(EnhancePirWork::Query(request))
            | TransactionEnhancementWork::Private(EnhancePirWork::Suspended(
                EnhancePirSuspension::OutgoingNotRecoverable(request)
                | EnhancePirSuspension::Rejected(request),
            )) => Some(request.request_id().txid()),
            TransactionEnhancementWork::Private(EnhancePirWork::Suspended(
                EnhancePirSuspension::Discovery(failure),
            )) => Some(failure.txid),
            // Rediscovery names a block, not a transaction.
            TransactionEnhancementWork::Private(EnhancePirWork::Rediscover(_)) => None,
        })
        .collect()
}

fn details(pending: bool, stored: bool, recorded: bool, shielded: bool) -> TransactionDetails {
    if pending {
        TransactionDetails::Pending
    } else if stored || !recorded || !shielded {
        // A transaction the wallet never recorded has nothing left to recover,
        // and transparent outputs carry no memo or change to wait for.
        TransactionDetails::Complete
    } else {
        TransactionDetails::Unavailable
    }
}

impl RealWallet {
    /// Reads what the local wallet recorded about one transaction for the
    /// development accounts. Nothing is decrypted or fetched here.
    pub(crate) async fn wallet_transaction(&self, txid: TxId) -> Result<WalletTransaction> {
        let path = self.wallet_path.clone();
        let account_ids = self
            .account_ids
            .iter()
            .map(|id| id.expose_uuid())
            .collect::<Vec<_>>();
        self.with_db(move |wallet| {
            let pending = enhancement_txids(wallet.transaction_enhancement_work()?).contains(&txid);
            // A stored transaction that no longer parses is an error, not a
            // transaction whose details are unavailable.
            let stored = wallet
                .get_transaction(txid)
                .context("reading the stored transaction")?
                .is_some();
            let db = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            let bytes = *txid.as_ref();
            let (recorded, fee_zatoshi) = user_record(&db, &bytes, &account_ids)?;
            let outputs = wallet_outputs(&db, &bytes, &account_ids)?;
            let spends = wallet_spends(&db, &bytes, &account_ids)?;
            let shielded = outputs.iter().any(|output| output.pool != "transparent")
                || spends.iter().any(|spend| spend.pool != "transparent");
            Ok(WalletTransaction {
                fee_zatoshi,
                details: details(pending, stored, recorded, shielded),
                outputs,
                spends,
            })
        })
        .await
    }

    pub(crate) async fn canonical_activity(
        &self,
        limit: u32,
        journal_ids: Vec<String>,
    ) -> Result<(Vec<CanonicalActivity>, HashMap<String, String>)> {
        let path = self.wallet_path.clone();
        let account_ids = self
            .account_ids
            .iter()
            .map(|id| id.expose_uuid())
            .collect::<Vec<_>>();
        self.with_db(move |wallet| {
            let prepared_txids = prepared_payment_txids(wallet, &journal_ids)?;
            let db = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            Ok((
                read_canonical_activity(&db, &account_ids, limit)?,
                prepared_txids,
            ))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(to_account: Option<u8>, address: &str, value: u64) -> Output {
        Output {
            pool: Some("ironwood".to_owned()),
            from_account: None,
            to_account,
            to_treasury: false,
            address: Some(address.to_owned()),
            is_change: false,
            value,
            memo: None,
        }
    }

    #[test]
    fn one_account_is_optional_without_indexing_an_empty_list() {
        assert_eq!(only::<u8>(&[]), None);
        assert_eq!(only(&[2]), Some(2));
        assert_eq!(only(&[1, 2]), None);
    }

    #[test]
    fn one_transaction_preserves_every_affected_user_account() {
        let summary = summarize_outputs(&[
            output(Some(2), "uregtest1account2", 100_000_000),
            output(Some(3), "uregtest1account3", 200_000_000),
        ])
        .unwrap();

        assert_eq!(summary.direction, "incoming");
        assert_eq!(summary.to_account, None);
        assert_eq!(summary.to_accounts, [2, 3]);
        assert_eq!(summary.amount_zatoshi, 300_000_000);
    }

    #[test]
    fn treasury_recipient_is_excluded_from_user_activity() {
        let user = output(Some(2), "uregtest1account2", 100_000_000);
        let mut treasury = output(None, "uregtest1treasury", 400_000_000);
        treasury.to_treasury = true;
        let summary = summarize_outputs(&[user, treasury]).unwrap();

        assert_eq!(summary.to_accounts, [2]);
        assert_eq!(summary.recipient_addresses, ["uregtest1account2"]);
        assert!(summary.external_addresses.is_empty());
        assert_eq!(summary.amount_zatoshi, 100_000_000);
    }

    #[test]
    fn external_addresses_exclude_development_accounts() {
        let summary = summarize_outputs(&[
            output(Some(2), "uregtest1account2", 100_000_000),
            output(None, "uregtest1external", 200_000_000),
        ])
        .unwrap();

        assert_eq!(summary.to_accounts, [2]);
        assert_eq!(
            summary.recipient_addresses,
            ["uregtest1account2", "uregtest1external"]
        );
        assert_eq!(summary.external_addresses, ["uregtest1external"]);
    }

    #[test]
    fn paying_the_sender_and_an_external_address_is_outgoing() {
        let mut own = output(Some(1), "tmaccount1", 100_000_000);
        own.from_account = Some(1);
        let mut external = output(None, "uregtest1external", 200_000_000);
        external.from_account = Some(1);

        let summary = summarize_outputs(&[own.clone(), external]).unwrap();
        assert_eq!(summary.direction, "outgoing");
        assert_eq!(summary.to_accounts, [1]);
        assert_eq!(summary.external_addresses, ["uregtest1external"]);

        assert_eq!(summarize_outputs(&[own]).unwrap().direction, "self");
    }

    #[test]
    fn text_memo_decodes_the_sdks_unpadded_storage_representation() {
        assert_eq!(text_memo(Some(b"hello".to_vec())).as_deref(), Some("hello"));
    }

    #[test]
    fn a_non_text_memo_is_the_stored_bytes_as_hex() {
        assert_eq!(
            stored_memo(Some(vec![0xff, 0x01])),
            Some(StoredMemo::Hex("ff01".to_owned()))
        );
        assert_eq!(text_memo(Some(vec![0xff, 0x01])), None);
    }

    #[test]
    fn an_empty_or_missing_memo_is_none() {
        assert_eq!(stored_memo(Some(vec![0xf6])), None);
        assert_eq!(stored_memo(None), None);
    }

    /// Accounts 1-5, then the treasury, as the server orders them.
    fn accounts() -> Vec<Uuid> {
        let mut ids = (1..=5).map(Uuid::from_u128).collect::<Vec<_>>();
        ids.push(Uuid::from_u128(6));
        ids
    }

    /// Pool, sender and recipient account numbers, change flag, and stored memo.
    type OutputRow<'a> = (i64, Option<u128>, Option<u128>, bool, &'a [u8]);

    fn outputs_table(rows: &[OutputRow]) -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE v_tx_outputs (txid BLOB, output_pool INTEGER, output_index INTEGER,
                 from_account_uuid BLOB, to_account_uuid BLOB, to_address TEXT,
                 is_change INTEGER, value INTEGER, memo BLOB);
             CREATE TABLE accounts (id INTEGER, uuid BLOB);
             CREATE TABLE addresses (account_id INTEGER, address TEXT,
                 cached_transparent_receiver_address TEXT);",
        )
        .unwrap();
        for (index, (pool, from, to, is_change, memo)) in rows.iter().enumerate() {
            db.execute(
                "INSERT INTO v_tx_outputs VALUES (?1,?2,?3,?4,?5,NULL,?6,1000,?7)",
                params![
                    [7u8; 32],
                    pool,
                    index,
                    from.map(Uuid::from_u128),
                    to.map(Uuid::from_u128),
                    is_change,
                    (!memo.is_empty()).then(|| memo.to_vec()),
                ],
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn change_paid_to_another_account_is_a_transfer() {
        let db = outputs_table(&[(4, Some(1), Some(2), true, b"\xf6")]);
        let outputs = wallet_outputs(&db, &[7; 32], &accounts()).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].from, Some(Party::Account(1)));
        assert_eq!(outputs[0].to, Some(Party::Account(2)));
        assert!(!outputs[0].change);
        assert_eq!(outputs[0].memo, None);
    }

    #[test]
    fn faucet_payments_show_the_faucet_and_hide_its_change() {
        let db = outputs_table(&[
            (4, Some(6), Some(2), false, b"hi"),
            (4, Some(6), Some(6), true, b""),
            (0, Some(6), None, false, b""),
        ]);
        let outputs = wallet_outputs(&db, &[7; 32], &accounts()).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].from, Some(Party::Faucet));
        assert_eq!(outputs[0].to, Some(Party::Account(2)));
        assert_eq!(outputs[0].memo, Some(StoredMemo::Text("hi".to_owned())));
        assert_eq!(
            serde_json::to_value(&outputs[0]).unwrap()["from"],
            serde_json::json!("faucet")
        );
    }

    #[test]
    fn a_payment_to_the_faucet_address_has_no_development_recipient() {
        let db = outputs_table(&[(0, Some(1), Some(6), false, b"")]);
        let outputs = wallet_outputs(&db, &[7; 32], &accounts()).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].pool, "transparent");
        assert_eq!(outputs[0].to, None);
        assert!(!outputs[0].change);
    }

    #[test]
    fn an_unscanned_payment_to_another_account_names_that_account() {
        let db = outputs_table(&[]);
        db.execute("INSERT INTO accounts VALUES (2, ?1)", [Uuid::from_u128(2)])
            .unwrap();
        db.execute_batch(
            "INSERT INTO addresses VALUES (2, 'uregtest1two', 'tmTwo');
             INSERT INTO v_tx_outputs VALUES
                 (x'0707070707070707070707070707070707070707070707070707070707070707',
                  4, 0, NULL, NULL, 'uregtest1two', 0, 1000, NULL),
                 (x'0707070707070707070707070707070707070707070707070707070707070707',
                  0, 0, NULL, NULL, 'tmTwo', 0, 1000, NULL),
                 (x'0707070707070707070707070707070707070707070707070707070707070707',
                  4, 1, NULL, NULL, 'uregtest1elsewhere', 0, 1000, NULL);",
        )
        .unwrap();
        db.execute(
            "UPDATE v_tx_outputs SET from_account_uuid=?1",
            [Uuid::from_u128(1)],
        )
        .unwrap();
        let outputs = wallet_outputs(&db, &[7; 32], &accounts()).unwrap();
        let recipients = outputs.iter().map(|output| output.to).collect::<Vec<_>>();
        assert_eq!(
            recipients,
            [Some(Party::Account(2)), Some(Party::Account(2)), None]
        );
    }

    #[test]
    fn details_are_complete_only_when_the_wallet_stored_the_transaction() {
        assert_eq!(details(true, true, true, true), TransactionDetails::Pending);
        assert_eq!(
            details(false, true, true, true),
            TransactionDetails::Complete
        );
        assert_eq!(
            details(false, false, true, true),
            TransactionDetails::Unavailable
        );
        assert_eq!(
            details(false, false, false, true),
            TransactionDetails::Complete
        );
        // Transparent outputs have no memo or change flag left to recover.
        assert_eq!(
            details(false, false, true, false),
            TransactionDetails::Complete
        );
    }

    #[test]
    fn oversized_or_invalid_memos_fall_back_to_hex() {
        assert_eq!(
            stored_memo(Some(vec![b'a'; 513])),
            Some(StoredMemo::Hex("61".repeat(513)))
        );
        assert_eq!(
            stored_memo(Some(vec![0xc3, 0x28])),
            Some(StoredMemo::Hex("c328".to_owned()))
        );
        // An explicit empty text memo is stored as no bytes.
        assert_eq!(
            stored_memo(Some(vec![])),
            Some(StoredMemo::Text(String::new()))
        );
    }
}
