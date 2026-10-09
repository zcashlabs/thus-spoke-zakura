use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use bip39::Mnemonic;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use uuid::Uuid;
use zcash_keys::{
    address::Address,
    keys::{Era, UnifiedAddressRequest, UnifiedFullViewingKey, UnifiedSpendingKey},
};
use zcash_protocol::local_consensus::LocalNetwork;

pub const ZATOSHIS_PER_ZEC: u64 = 100_000_000;
pub const USER_ACCOUNT_COUNT: u8 = 5;
pub const TREASURY_ACCOUNT_ID: u8 = 6;
/// The order must match `row_account`.
const ACCOUNT_COLUMNS: &str =
    "id,name,unified_address,transparent_address,transparent_zatoshi,ironwood_zatoshi";
/// Qualified so it also reads unambiguously in joins. The order must match `row_activity`.
const ACTIVITY_COLUMNS: &str = "a.id,a.kind,a.from_account,a.to_account,a.source_pool,a.destination_pool,a.amount_zatoshi,a.txid,a.block_hash,a.status,a.created_at";
/// The order must match `row_address_faucet`.
const ADDRESS_FAUCET_COLUMNS: &str = "id,address,amount_zatoshi,txid,block_hash,status";

#[derive(Debug, thiserror::Error)]
#[error("idempotency key was already used for a different payment")]
pub struct IdempotencyConflict;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreasuryCursor {
    pub receiver: String,
    pub height: u32,
    pub block_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Account {
    pub id: u8,
    pub name: String,
    pub unified_address: String,
    pub transparent_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unified_full_viewing_key: Option<String>,
    pub transparent_zatoshi: u64,
    pub ironwood_zatoshi: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevelopmentAccountSecret {
    pub id: u8,
    pub unified_address: String,
    pub unified_spending_key_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevelopmentSecrets {
    pub mnemonic: String,
    pub accounts: Vec<DevelopmentAccountSecret>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Activity {
    pub id: String,
    pub kind: String,
    pub from_account: Option<u8>,
    pub to_account: u8,
    pub source_pool: String,
    pub destination_pool: String,
    pub amount_zatoshi: u64,
    pub txid: String,
    pub block_hash: Option<String>,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressFaucet {
    pub id: String,
    pub address: String,
    pub amount_zatoshi: u64,
    pub txid: String,
    pub block_hash: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedTransaction {
    pub raw_transaction: Vec<u8>,
    pub expiry_height: u64,
}

#[derive(Clone)]
pub struct Store(Arc<Mutex<Connection>>);

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Ok(Self(Arc::new(Mutex::new(connection))))
    }

    pub fn initialize(&self) -> Result<()> {
        let db = self.0.lock().unwrap();
        db.execute_batch(r#"
            CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS accounts (
                id INTEGER PRIMARY KEY, name TEXT NOT NULL, unified_address TEXT NOT NULL,
                transparent_address TEXT NOT NULL, transparent_zatoshi INTEGER NOT NULL DEFAULT 0,
                ironwood_zatoshi INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS activity (
                id TEXT PRIMARY KEY, kind TEXT NOT NULL, from_account INTEGER, to_account INTEGER NOT NULL,
                source_pool TEXT NOT NULL, destination_pool TEXT NOT NULL, amount_zatoshi INTEGER NOT NULL,
                txid TEXT NOT NULL, block_hash TEXT, status TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS idempotency (
                key TEXT PRIMARY KEY, activity_id TEXT NOT NULL, memo TEXT
            );
            CREATE TABLE IF NOT EXISTS prepared_payments (
                activity_id TEXT PRIMARY KEY, raw_transaction BLOB NOT NULL,
                expiry_height INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS address_faucets (
                key TEXT PRIMARY KEY, id TEXT UNIQUE NOT NULL, address TEXT NOT NULL,
                amount_zatoshi INTEGER NOT NULL, txid TEXT NOT NULL DEFAULT '',
                block_hash TEXT, status TEXT NOT NULL DEFAULT 'preparing'
            );
        "#)?;
        let has_memo = db
            .prepare("PRAGMA table_info(idempotency)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|column| column == "memo");
        if !has_memo {
            db.execute("ALTER TABLE idempotency ADD COLUMN memo TEXT", [])?;
        }
        let stored_seed = db
            .query_row("SELECT value FROM metadata WHERE key='seed'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        let seed = if let Some(seed) = stored_seed {
            hex::decode(seed).context("invalid wallet seed")?
        } else {
            let entropy = [0u8; 32];
            let mnemonic = Mnemonic::from_entropy(&entropy)
                .context("encoding wallet entropy as a BIP-39 mnemonic")?;
            let seed = mnemonic.to_seed("");
            db.execute(
                "INSERT INTO metadata(key, value) VALUES('seed', ?1)",
                [hex::encode(seed)],
            )?;
            db.execute(
                "INSERT INTO metadata(key, value) VALUES('mnemonic', ?1)",
                [mnemonic.to_string()],
            )?;
            seed.to_vec()
        };
        for id in 1u8..=TREASURY_ACCOUNT_ID {
            let (ua, taddr) = derived_addresses(&seed, id)?;
            db.execute("INSERT OR IGNORE INTO accounts(id,name,unified_address,transparent_address) VALUES(?1,?2,?3,?4)", params![id, format!("Account {id}"), ua, taddr])?;
        }
        Ok(())
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        let mut accounts = {
            let db = self.0.lock().unwrap();
            let mut query = db.prepare(&format!(
                "SELECT {ACCOUNT_COLUMNS} FROM accounts ORDER BY id"
            ))?;
            query
                .query_map([], row_account)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let seed = hex::decode(self.seed()?).context("invalid wallet seed")?;
        let network = local_network();
        for account in &mut accounts {
            if (1..=USER_ACCOUNT_COUNT).contains(&account.id) {
                account.unified_full_viewing_key =
                    Some(derived_full_viewing_key(&seed, account.id)?.encode(&network));
            }
        }
        Ok(accounts)
    }

    #[cfg(test)]
    pub fn user_accounts(&self) -> Result<Vec<Account>> {
        Ok(self
            .accounts()?
            .into_iter()
            .filter(|account| account.id <= USER_ACCOUNT_COUNT)
            .collect())
    }

    pub fn account(&self, id: u8) -> Result<Account> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                &format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE id=?1"),
                [id],
                row_account,
            )
            .with_context(|| format!("account {id} does not exist"))
    }

    pub fn activities(&self, limit: u32) -> Result<Vec<Activity>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(&format!(
            "SELECT {ACTIVITY_COLUMNS} FROM activity a WHERE a.txid != '' ORDER BY a.rowid DESC LIMIT ?1"
        ))?;
        Ok(query
            .query_map([limit.min(100)], row_activity)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn unconfirmed_activities(&self) -> Result<Vec<Activity>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(&format!(
            "SELECT {ACTIVITY_COLUMNS} FROM activity a WHERE a.txid!='' AND a.status!='confirmed' ORDER BY a.rowid ASC"
        ))?;
        Ok(query
            .query_map([], row_activity)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn activity_for_key(&self, key: &str) -> Result<Option<Activity>> {
        let db = self.0.lock().unwrap();
        activity_for_key(&db, key)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn claim_transfer(
        &self,
        from: u8,
        to: u8,
        source_pool: &str,
        destination_pool: &str,
        amount: u64,
        key: &str,
        memo: Option<&str>,
    ) -> Result<Activity> {
        validate_pool(source_pool)?;
        validate_pool(destination_pool)?;
        let request = new_activity(
            "send",
            Some(from),
            to,
            source_pool,
            destination_pool,
            amount,
        );
        self.claim(request, key, memo)
    }

    pub fn claim_faucet(&self, to: u8, pool: &str, amount: u64, key: &str) -> Result<Activity> {
        validate_pool(pool)?;
        let request = new_activity("faucet", None, to, "ironwood", pool, amount);
        self.claim(request, key, None)
    }

    /// Returns the payment already claimed with `key`, or records `request` as a new
    /// `preparing` claim. Submission and recovery happen outside this function.
    fn claim(&self, request: Activity, key: &str, memo: Option<&str>) -> Result<Activity> {
        if request.amount_zatoshi == 0 {
            bail!("amount must be greater than zero");
        }
        // Hold the lock from lookup to insert so concurrent retries create one payment.
        let mut db = self.0.lock().unwrap();
        if let Some(activity) = activity_for_key(&db, key)? {
            let saved_memo: Option<String> =
                db.query_row("SELECT memo FROM idempotency WHERE key=?1", [key], |row| {
                    row.get(0)
                })?;
            // An absent memo and an empty memo are different payments.
            if !same_payment(&activity, &request) || saved_memo.as_deref() != memo {
                return Err(IdempotencyConflict.into());
            }
            return Ok(activity);
        }
        if address_for_key(&db, key)?.is_some() {
            return Err(IdempotencyConflict.into());
        }
        for id in request.from_account.into_iter().chain([request.to_account]) {
            if !db.query_row(
                "SELECT EXISTS(SELECT 1 FROM accounts WHERE id=?1)",
                [id],
                |row| row.get::<_, bool>(0),
            )? {
                bail!("account {id} does not exist");
            }
        }
        let tx = db.transaction()?;
        insert_activity(&tx, &request, key, memo)?;
        tx.commit()?;
        Ok(request)
    }

    pub fn address_faucet_for_key(&self, key: &str) -> Result<Option<AddressFaucet>> {
        let db = self.0.lock().unwrap();
        address_for_key(&db, key)
    }

    pub fn claim_address_faucet(
        &self,
        address: &str,
        amount: u64,
        key: &str,
    ) -> Result<AddressFaucet> {
        if amount == 0 {
            bail!("amount must be greater than zero");
        }
        let db = self.0.lock().unwrap();
        if let Some(payment) = address_for_key(&db, key)? {
            if payment.address != address || payment.amount_zatoshi != amount {
                return Err(IdempotencyConflict.into());
            }
            return Ok(payment);
        }
        if activity_for_key(&db, key)?.is_some() {
            return Err(IdempotencyConflict.into());
        }
        let id = Uuid::new_v4().to_string();
        db.execute(
            "INSERT INTO address_faucets(key,id,address,amount_zatoshi) VALUES(?1,?2,?3,?4)",
            params![key, id, address, amount],
        )?;
        address_for_key(&db, key)?.context("address faucet claim was not stored")
    }

    pub fn discard_address_preparing(&self, id: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "DELETE FROM address_faucets WHERE id=?1 AND status='preparing'",
            [id],
        )?;
        Ok(())
    }

    pub fn record_address_prepared(
        &self,
        id: &str,
        txid: &str,
        raw_transaction: &[u8],
        expiry_height: u64,
    ) -> Result<AddressFaucet> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        let updated = tx.execute(
            "UPDATE address_faucets SET txid=?1,status='prepared' WHERE id=?2 AND status='preparing'",
            params![txid, id],
        )?;
        let payment = address_by_id(&tx, id)?;
        if updated == 0
            && (payment.txid != txid
                || !matches!(
                    payment.status.as_str(),
                    "prepared" | "broadcast" | "confirmed"
                ))
        {
            bail!("address faucet {id} is not waiting for this prepared transaction");
        }
        tx.execute(
            "INSERT INTO prepared_payments(activity_id,raw_transaction,expiry_height) VALUES(?1,?2,?3) ON CONFLICT(activity_id) DO NOTHING",
            params![id, raw_transaction, expiry_height],
        )?;
        tx.commit()?;
        Ok(payment)
    }

    pub fn reset_address_for_retry(&self, id: &str, txid: &str) -> Result<AddressFaucet> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        if tx.execute(
            "UPDATE address_faucets SET txid='',block_hash=NULL,status='preparing' WHERE id=?1 AND txid=?2 AND status IN ('prepared','broadcast')",
            params![id, txid],
        )? == 1 {
            tx.execute("DELETE FROM prepared_payments WHERE activity_id=?1", [id])?;
        }
        let payment = address_by_id(&tx, id)?;
        tx.commit()?;
        Ok(payment)
    }

    pub fn mark_address_broadcast(&self, id: &str, txid: &str) -> Result<AddressFaucet> {
        let db = self.0.lock().unwrap();
        let updated = db.execute(
            "UPDATE address_faucets SET status='broadcast' WHERE id=?1 AND txid=?2 AND status IN ('prepared','broadcast')",
            params![id, txid],
        )?;
        let payment = address_by_id(&db, id)?;
        if updated == 0 && (payment.txid != txid || payment.status != "confirmed") {
            bail!("address faucet {id} has no matching prepared transaction");
        }
        Ok(payment)
    }

    pub fn confirm_address(&self, id: &str, txid: &str, block_hash: &str) -> Result<AddressFaucet> {
        if block_hash.is_empty() {
            bail!("block hash is required to confirm address faucet");
        }
        let db = self.0.lock().unwrap();
        db.execute(
            "UPDATE address_faucets SET status='confirmed',block_hash=?1 WHERE id=?2 AND txid=?3",
            params![block_hash, id, txid],
        )?;
        address_by_id(&db, id)
    }

    pub fn discard_preparing(&self, id: &str) -> Result<()> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        if tx.execute(
            "DELETE FROM activity WHERE id=?1 AND status='preparing'",
            [id],
        )? == 1
        {
            tx.execute("DELETE FROM idempotency WHERE activity_id=?1", [id])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn record_prepared(
        &self,
        id: &str,
        txid: &str,
        raw_transaction: &[u8],
        expiry_height: u64,
    ) -> Result<Activity> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        let updated = tx.execute(
            "UPDATE activity SET txid=?1,status='prepared' WHERE id=?2 AND status='preparing'",
            params![txid, id],
        )?;
        let activity = activity_by_id(&tx, id)?;
        if updated == 0
            && (activity.txid != txid
                || !matches!(
                    activity.status.as_str(),
                    "prepared" | "broadcast" | "confirmed"
                ))
        {
            bail!("payment {id} is not waiting for this prepared transaction");
        }
        tx.execute(
            "INSERT INTO prepared_payments(activity_id,raw_transaction,expiry_height) VALUES(?1,?2,?3) ON CONFLICT(activity_id) DO NOTHING",
            params![id, raw_transaction, expiry_height],
        )?;
        tx.commit()?;
        Ok(activity)
    }

    pub fn prepared_transaction(&self, id: &str) -> Result<PreparedTransaction> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT raw_transaction,expiry_height FROM prepared_payments WHERE activity_id=?1",
                [id],
                |row| {
                    Ok(PreparedTransaction {
                        raw_transaction: row.get(0)?,
                        expiry_height: row.get(1)?,
                    })
                },
            )
            .with_context(|| format!("prepared transaction {id} is missing"))
    }

    pub fn reset_for_retry(&self, id: &str, txid: &str) -> Result<Activity> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        let updated = tx.execute(
            "UPDATE activity SET txid='',block_hash=NULL,status='preparing' WHERE id=?1 AND txid=?2 AND status IN ('prepared','broadcast')",
            params![id, txid],
        )?;
        if updated == 1 {
            tx.execute("DELETE FROM prepared_payments WHERE activity_id=?1", [id])?;
        }
        let activity = activity_by_id(&tx, id)?;
        tx.commit()?;
        Ok(activity)
    }

    pub fn mark_broadcast(&self, id: &str, txid: &str) -> Result<Activity> {
        let db = self.0.lock().unwrap();
        let updated = db.execute(
            "UPDATE activity SET status='broadcast' WHERE id=?1 AND txid=?2 AND status IN ('prepared','broadcast')",
            params![id, txid],
        )?;
        let activity = activity_by_id(&db, id)?;
        if updated == 0 && (activity.txid != txid || activity.status != "confirmed") {
            bail!("payment {id} has no matching prepared transaction");
        }
        Ok(activity)
    }

    pub fn confirm(&self, id: &str, txid: &str, block_hash: &str) -> Result<Activity> {
        if block_hash.is_empty() {
            bail!("block hash is required to confirm activity");
        }
        let db = self.0.lock().unwrap();
        db.execute(
            "UPDATE activity SET status='confirmed',block_hash=?1 WHERE id=?2 AND txid=?3",
            params![block_hash, id, txid],
        )?;
        activity_by_id(&db, id).map_err(Into::into)
    }

    pub fn seed(&self) -> Result<String> {
        Ok(self.0.lock().unwrap().query_row(
            "SELECT value FROM metadata WHERE key='seed'",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn mnemonic(&self) -> Result<String> {
        self.0
            .lock()
            .unwrap()
            .query_row("SELECT value FROM metadata WHERE key='mnemonic'", [], |r| {
                r.get(0)
            })
            .context("wallet mnemonic is missing; restart this disposable environment")
    }

    pub fn development_secrets(&self) -> Result<DevelopmentSecrets> {
        let seed = hex::decode(self.seed()?).context("invalid wallet seed")?;
        let mnemonic = self.mnemonic()?;
        let network = local_network();
        let mut accounts = Vec::with_capacity(usize::from(USER_ACCOUNT_COUNT));
        for id in 1..=USER_ACCOUNT_COUNT {
            let account_index = zip32::AccountId::try_from(u32::from(id - 1))
                .map_err(|_| anyhow::anyhow!("invalid ZIP-32 account {id}"))?;
            let spending_key = UnifiedSpendingKey::from_seed(&network, &seed, account_index)
                .map_err(|error| anyhow::anyhow!("deriving account {id}: {error:?}"))?;
            accounts.push(DevelopmentAccountSecret {
                id,
                unified_address: self.account(id)?.unified_address,
                unified_spending_key_hex: hex::encode(spending_key.to_bytes(Era::Orchard)),
            });
        }
        Ok(DevelopmentSecrets { mnemonic, accounts })
    }
}

fn insert_activity(db: &Connection, a: &Activity, key: &str, memo: Option<&str>) -> Result<()> {
    db.execute("INSERT INTO activity(id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,status) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![a.id,a.kind,a.from_account,a.to_account,a.source_pool,a.destination_pool,a.amount_zatoshi,a.txid,a.status])?;
    db.execute(
        "INSERT INTO idempotency(key,activity_id,memo) VALUES(?1,?2,?3)",
        params![key, a.id, memo],
    )?;
    Ok(())
}

fn activity_by_id(db: &Connection, id: &str) -> rusqlite::Result<Activity> {
    db.query_row(
        &format!("SELECT {ACTIVITY_COLUMNS} FROM activity a WHERE a.id=?1"),
        [id],
        row_activity,
    )
}

fn activity_for_key(db: &Connection, key: &str) -> Result<Option<Activity>> {
    db.query_row(
        &format!(
            "SELECT {ACTIVITY_COLUMNS} FROM activity a JOIN idempotency i ON i.activity_id=a.id WHERE i.key=?1"
        ),
        [key],
        row_activity,
    )
    .optional()
    .map_err(Into::into)
}
fn address_for_key(db: &Connection, key: &str) -> Result<Option<AddressFaucet>> {
    db.query_row(
        &format!("SELECT {ADDRESS_FAUCET_COLUMNS} FROM address_faucets WHERE key=?1"),
        [key],
        row_address_faucet,
    )
    .optional()
    .map_err(Into::into)
}
fn address_by_id(db: &Connection, id: &str) -> Result<AddressFaucet> {
    db.query_row(
        &format!("SELECT {ADDRESS_FAUCET_COLUMNS} FROM address_faucets WHERE id=?1"),
        [id],
        row_address_faucet,
    )
    .map_err(Into::into)
}
fn row_address_faucet(row: &rusqlite::Row<'_>) -> rusqlite::Result<AddressFaucet> {
    Ok(AddressFaucet {
        id: row.get(0)?,
        address: row.get(1)?,
        amount_zatoshi: row.get(2)?,
        txid: row.get(3)?,
        block_hash: row.get(4)?,
        status: row.get(5)?,
    })
}
fn new_activity(
    kind: &str,
    from: Option<u8>,
    to: u8,
    source: &str,
    destination: &str,
    amount: u64,
) -> Activity {
    let id = Uuid::new_v4().to_string();
    Activity {
        id,
        kind: kind.into(),
        from_account: from,
        to_account: to,
        source_pool: source.into(),
        destination_pool: destination.into(),
        amount_zatoshi: amount,
        txid: String::new(),
        block_hash: None,
        status: "preparing".into(),
        created_at: String::new(),
    }
}
fn validate_pool(pool: &str) -> Result<()> {
    if matches!(pool, "transparent" | "ironwood") {
        Ok(())
    } else {
        bail!("pool must be transparent or ironwood")
    }
}

fn same_payment(existing: &Activity, request: &Activity) -> bool {
    existing.kind == request.kind
        && existing.from_account == request.from_account
        && existing.to_account == request.to_account
        && existing.source_pool == request.source_pool
        && existing.destination_pool == request.destination_pool
        && existing.amount_zatoshi == request.amount_zatoshi
}
fn row_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: row.get(0)?,
        name: row.get(1)?,
        unified_address: row.get(2)?,
        transparent_address: row.get(3)?,
        unified_full_viewing_key: None,
        transparent_zatoshi: row.get(4)?,
        ironwood_zatoshi: row.get(5)?,
    })
}
fn row_activity(row: &rusqlite::Row<'_>) -> rusqlite::Result<Activity> {
    Ok(Activity {
        id: row.get(0)?,
        kind: row.get(1)?,
        from_account: row.get(2)?,
        to_account: row.get(3)?,
        source_pool: row.get(4)?,
        destination_pool: row.get(5)?,
        amount_zatoshi: row.get(6)?,
        txid: row.get(7)?,
        block_hash: row.get(8)?,
        status: row.get(9)?,
        created_at: row.get(10)?,
    })
}
fn derived_full_viewing_key(seed: &[u8], id: u8) -> Result<UnifiedFullViewingKey> {
    let index = id.checked_sub(1).context("invalid account id")?;
    let account = zip32::AccountId::try_from(u32::from(index))
        .map_err(|_| anyhow::anyhow!("invalid ZIP-32 account {id}"))?;
    let usk = UnifiedSpendingKey::from_seed(&local_network(), seed, account)
        .map_err(|error| anyhow::anyhow!("deriving account {id}: {error:?}"))?;
    Ok(usk.to_unified_full_viewing_key())
}

fn derived_addresses(seed: &[u8], id: u8) -> Result<(String, String)> {
    let network = local_network();
    let (ua, _) = derived_full_viewing_key(seed, id)?
        .default_address(UnifiedAddressRequest::AllAvailableKeys)
        .map_err(|error| anyhow::anyhow!("deriving account {id} address: {error:?}"))?;
    let transparent = ua
        .transparent()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("account {id} has no transparent receiver"))?;
    Ok((
        ua.encode(&network),
        Address::Transparent(transparent).encode(&network),
    ))
}

fn local_network() -> LocalNetwork {
    crate::wallet::regtest_network()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    #[test]
    fn address_faucet_replay_survives_restart_and_rejects_conflicting_intents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.db");
        let store = Store::open(&path).unwrap();
        store.initialize().unwrap();
        let address = store.account(1).unwrap().unified_address;
        let first = store
            .claim_address_faucet(&address, 100_000_000, "address-key")
            .unwrap();
        let first = store
            .record_address_prepared(&first.id, "txid-one", b"signed bytes", 140)
            .unwrap();
        assert_eq!(first.status, "prepared");
        drop(store);

        let reopened = Store::open(&path).unwrap();
        reopened.initialize().unwrap();
        let replay = reopened
            .claim_address_faucet(&address, 100_000_000, "address-key")
            .unwrap();
        assert_eq!(replay.id, first.id);
        assert_eq!(replay.txid, "txid-one");
        assert_eq!(
            reopened
                .prepared_transaction(&replay.id)
                .unwrap()
                .raw_transaction,
            b"signed bytes"
        );
        assert!(
            reopened
                .claim_address_faucet(&address, 2, "address-key")
                .is_err()
        );
        assert!(
            reopened
                .claim_address_faucet(
                    &reopened.account(2).unwrap().unified_address,
                    100_000_000,
                    "address-key"
                )
                .is_err()
        );
        assert!(
            reopened
                .claim_faucet(1, "ironwood", 100_000_000, "address-key")
                .is_err()
        );
        assert!(
            reopened
                .claim_address_faucet(&address, 100_000_000, "fresh-key")
                .is_ok()
        );
        reopened
            .claim_faucet(1, "ironwood", 100_000_000, "account-key")
            .unwrap();
        assert!(
            reopened
                .claim_address_faucet(&address, 100_000_000, "account-key")
                .is_err()
        );
    }
    #[test]
    fn concurrent_address_faucet_claims_share_one_operation() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let claims: Vec<_> = (0..2)
            .map(|_| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.claim_address_faucet("same-address", 1, "same-operation")
                })
            })
            .collect();
        barrier.wait();
        let ids: Vec<_> = claims
            .into_iter()
            .map(|claim| claim.join().unwrap().unwrap().id)
            .collect();
        assert_eq!(ids[0], ids[1]);
    }
    #[test]
    fn creates_user_accounts_and_hidden_treasury() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        assert_eq!(store.accounts().unwrap().len(), 6);
        assert_eq!(store.user_accounts().unwrap().len(), 5);
        assert_eq!(
            store.account(TREASURY_ACCOUNT_ID).unwrap().name,
            "Account 6"
        );
        assert!(
            store
                .account(1)
                .unwrap()
                .transparent_address
                .starts_with("tm")
        );
        let first = store
            .claim_faucet(2, "ironwood", ZATOSHIS_PER_ZEC, "same")
            .unwrap();
        let second = store
            .claim_faucet(2, "ironwood", ZATOSHIS_PER_ZEC, "same")
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(store.account(2).unwrap().ironwood_zatoshi, 0);
    }
    #[test]
    fn fresh_stores_derive_the_same_development_accounts() {
        let first = Store::open(":memory:").unwrap();
        first.initialize().unwrap();
        let second = Store::open(":memory:").unwrap();
        second.initialize().unwrap();
        assert_eq!(first.accounts().unwrap(), second.accounts().unwrap());
        assert_eq!(
            first.mnemonic().unwrap(),
            format!("{}art", "abandon ".repeat(23))
        );
        assert_eq!(
            first.account(1).unwrap().transparent_address,
            "tmBsTi2xWTjUdEXnuTceL7fecEQKeWaPDJd"
        );
    }
    #[test]
    fn restores_the_hidden_treasury_for_existing_stores() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        store
            .0
            .lock()
            .unwrap()
            .execute("DELETE FROM accounts WHERE id=?1", [TREASURY_ACCOUNT_ID])
            .unwrap();
        store.initialize().unwrap();
        assert_eq!(store.accounts().unwrap().len(), 6);
        assert_eq!(store.user_accounts().unwrap().len(), 5);
    }
    #[test]
    fn existing_store_exports_user_viewing_keys_without_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.db");
        let seed = [7u8; 32];
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE accounts (
                     id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                     unified_address TEXT NOT NULL, transparent_address TEXT NOT NULL,
                     transparent_zatoshi INTEGER NOT NULL DEFAULT 0,
                     ironwood_zatoshi INTEGER NOT NULL DEFAULT 0
                 );",
            )
            .unwrap();
            db.execute(
                "INSERT INTO metadata(key,value) VALUES('seed',?1)",
                [hex::encode(seed)],
            )
            .unwrap();
            for id in 1..=TREASURY_ACCOUNT_ID {
                let (ua, taddr) = derived_addresses(&seed, id).unwrap();
                db.execute(
                    "INSERT INTO accounts
                     (id,name,unified_address,transparent_address,transparent_zatoshi,ironwood_zatoshi)
                     VALUES(?1,?2,?3,?4,123,456)",
                    params![id, format!("Account {id}"), ua, taddr],
                )
                .unwrap();
            }
        }

        let store = Store::open(&path).unwrap();
        let users = store.user_accounts().unwrap();
        assert_eq!(
            users.iter().map(|a| a.id).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
        let network = local_network();
        let mut distinct = std::collections::HashSet::new();
        for account in &users {
            let json = serde_json::to_value(account).unwrap();
            let encoded = json["unified_full_viewing_key"]
                .as_str()
                .expect("user projection must contain a viewing key");
            let viewing = zcash_keys::keys::UnifiedFullViewingKey::decode(&network, encoded)
                .unwrap_or_else(|_| panic!("user viewing key must decode for regtest"));
            let (ua, _) = viewing
                .default_address(UnifiedAddressRequest::AllAvailableKeys)
                .unwrap();
            assert!(ua.encode(&network) == account.unified_address);
            let transparent = ua.transparent().cloned().expect("transparent receiver");
            assert!(
                Address::Transparent(transparent).encode(&network) == account.transparent_address
            );
            let index = zip32::AccountId::try_from(u32::from(account.id - 1)).unwrap();
            let expected = UnifiedSpendingKey::from_seed(&network, &seed, index)
                .unwrap()
                .to_unified_full_viewing_key()
                .encode(&network);
            assert!(encoded == expected);
            assert!(distinct.insert(encoded.to_owned()));
            assert_eq!(account.transparent_zatoshi, 123);
            assert_eq!(account.ironwood_zatoshi, 456);
        }

        let all = store.accounts().unwrap();
        assert_eq!(all.len(), 6);
        let treasury = all.iter().find(|a| a.id == TREASURY_ACCOUNT_ID).unwrap();
        assert!(
            serde_json::to_value(treasury)
                .unwrap()
                .get("unified_full_viewing_key")
                .is_none()
        );
        assert!(
            serde_json::to_value(store.account(TREASURY_ACCOUNT_ID).unwrap())
                .unwrap()
                .get("unified_full_viewing_key")
                .is_none()
        );
        store.initialize().unwrap();
        assert!(users == store.user_accounts().unwrap());
        {
            let db = store.0.lock().unwrap();
            let columns = db
                .prepare("PRAGMA table_info(accounts)")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(
                columns,
                [
                    "id",
                    "name",
                    "unified_address",
                    "transparent_address",
                    "transparent_zatoshi",
                    "ironwood_zatoshi",
                ]
            );
            let metadata_keys = db
                .prepare("SELECT key FROM metadata ORDER BY key")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(metadata_keys, ["seed"]);
        }
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert!(users == reopened.user_accounts().unwrap());
    }
    #[test]
    fn records_real_transfer_without_mutating_balances() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let activity = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "send", None)
            .unwrap();
        let activity = store
            .record_prepared(&activity.id, "real-txid", b"raw transaction", 140)
            .unwrap();
        let activity = store.mark_broadcast(&activity.id, &activity.txid).unwrap();
        assert_eq!(activity.txid, "real-txid");
        assert_eq!(store.account(1).unwrap().transparent_zatoshi, 0);
    }

    #[test]
    fn unconfirmed_activities_skip_unprepared_and_confirmed_rows() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        store
            .claim_transfer(1, 2, "ironwood", "ironwood", 11_000, "preparing-key", None)
            .unwrap();
        let pending = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "pending-key", None)
            .unwrap();
        let pending = store
            .record_prepared(&pending.id, "txid-pending", b"pending", 140)
            .unwrap();
        let pending = store.mark_broadcast(&pending.id, &pending.txid).unwrap();
        let mined = store
            .claim_transfer(1, 3, "ironwood", "ironwood", 13_000, "mined-key", None)
            .unwrap();
        let mined = store
            .record_prepared(&mined.id, "txid-mined", b"mined", 140)
            .unwrap();
        let mined = store.mark_broadcast(&mined.id, &mined.txid).unwrap();
        store
            .confirm(&mined.id, &mined.txid, &"c".repeat(64))
            .unwrap();

        let open = store.unconfirmed_activities().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, pending.id);
        assert_eq!(open[0].status, "broadcast");
        assert_eq!(open[0].txid, "txid-pending");
    }

    #[test]
    fn confirm_rejects_an_empty_block_hash() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let pending = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "empty-hash", None)
            .unwrap();
        let pending = store
            .record_prepared(&pending.id, "txid-empty", b"raw", 140)
            .unwrap();
        let pending = store.mark_broadcast(&pending.id, &pending.txid).unwrap();

        assert!(store.confirm(&pending.id, &pending.txid, "").is_err());
        let again = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "empty-hash", None)
            .unwrap();
        assert_eq!(again.id, pending.id);
        assert_eq!(again.status, "broadcast");
        assert_eq!(again.block_hash, None);
    }

    #[test]
    fn rejects_reusing_a_key_for_a_different_payment() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();

        let error = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 13_000, "same", None)
            .unwrap_err();

        assert!(error.to_string().contains("different payment"));
        assert!(error.downcast_ref::<IdempotencyConflict>().is_some());

        let error = store
            .claim_faucet(2, "ironwood", 12_000, "same")
            .unwrap_err();
        assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
    }

    #[test]
    fn reusing_a_key_conflicts_on_every_payment_field() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "send", None)
            .unwrap();
        store.claim_faucet(2, "ironwood", 12_000, "faucet").unwrap();

        for (from, to, source, destination, amount) in [
            (3, 2, "ironwood", "ironwood", 12_000),
            (1, 3, "ironwood", "ironwood", 12_000),
            (1, 2, "transparent", "ironwood", 12_000),
            (1, 2, "ironwood", "transparent", 12_000),
            (1, 2, "ironwood", "ironwood", 13_000),
        ] {
            let error = store
                .claim_transfer(from, to, source, destination, amount, "send", None)
                .unwrap_err();
            assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
        }
        for (to, pool, amount) in [
            (3, "ironwood", 12_000),
            (2, "transparent", 12_000),
            (2, "ironwood", 13_000),
        ] {
            let error = store.claim_faucet(to, pool, amount, "faucet").unwrap_err();
            assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
        }
        let error = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "faucet", None)
            .unwrap_err();
        assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
    }

    #[test]
    fn send_and_faucet_validate_their_own_requests() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();

        for (from, to, source, destination, amount) in [
            (9, 2, "ironwood", "ironwood", 12_000),
            (1, 9, "ironwood", "ironwood", 12_000),
            (1, 2, "sapling", "ironwood", 12_000),
            (1, 2, "ironwood", "sapling", 12_000),
            (1, 2, "ironwood", "ironwood", 0),
        ] {
            assert!(
                store
                    .claim_transfer(from, to, source, destination, amount, "send", None)
                    .is_err()
            );
        }
        for (to, pool, amount) in [
            (9, "ironwood", 12_000),
            (2, "sapling", 12_000),
            (2, "ironwood", 0),
        ] {
            assert!(store.claim_faucet(to, pool, amount, "faucet").is_err());
        }
        assert!(store.activity_for_key("send").unwrap().is_none());
        assert!(store.activity_for_key("faucet").unwrap().is_none());

        let faucet = store
            .claim_faucet(2, "transparent", 12_000, "faucet")
            .unwrap();
        assert_eq!(faucet.kind, "faucet");
        assert_eq!(faucet.from_account, None);
        assert_eq!(faucet.source_pool, "ironwood");
        assert_eq!(faucet.destination_pool, "transparent");
        assert_eq!(faucet.status, "preparing");
        assert!(faucet.txid.is_empty());
    }

    #[test]
    fn memo_is_part_of_the_claimed_payment() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let first = store
            .claim_transfer(
                1,
                2,
                "ironwood",
                "ironwood",
                12_000,
                "memo-key",
                Some("rent"),
            )
            .unwrap();
        assert_eq!(
            store
                .claim_transfer(
                    1,
                    2,
                    "ironwood",
                    "ironwood",
                    12_000,
                    "memo-key",
                    Some("rent")
                )
                .unwrap()
                .id,
            first.id
        );
        for memo in [None, Some(""), Some("gift")] {
            let error = store
                .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "memo-key", memo)
                .unwrap_err();
            assert!(error.downcast_ref::<IdempotencyConflict>().is_some());
        }
    }

    #[test]
    fn existing_idempotency_tables_gain_a_memo_column() {
        let store = Store::open(":memory:").unwrap();
        store
            .0
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TABLE idempotency (key TEXT PRIMARY KEY, activity_id TEXT NOT NULL)",
            )
            .unwrap();
        store.initialize().unwrap();
        store
            .claim_transfer(
                1,
                2,
                "ironwood",
                "ironwood",
                12_000,
                "memo-key",
                Some("rent"),
            )
            .unwrap();
        assert_eq!(
            store
                .0
                .lock()
                .unwrap()
                .query_row(
                    "SELECT memo FROM idempotency WHERE key='memo-key'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "rent"
        );
    }

    #[test]
    fn concurrent_claims_create_one_payment() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let threads = (0..2)
            .map(|_| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let claims = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(claims[0].id, claims[1].id);
        assert_eq!(claims[0].status, "preparing");
        assert!(store.activities(10).unwrap().is_empty());
    }

    #[test]
    fn discarding_a_failed_prepare_removes_its_claim() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let failed = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();

        store.discard_preparing(&failed.id).unwrap();

        assert!(store.activity_for_key("same").unwrap().is_none());
        let retry = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();
        assert_ne!(retry.id, failed.id);
    }

    #[test]
    fn prepared_payment_can_be_reset_after_expiry() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let claim = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();
        store
            .record_prepared(&claim.id, "expired-txid", b"signed transaction", 140)
            .unwrap();

        let retry = store.reset_for_retry(&claim.id, "expired-txid").unwrap();

        assert_eq!(retry.status, "preparing");
        assert!(retry.txid.is_empty());
        assert!(store.prepared_transaction(&claim.id).is_err());
        assert!(store.activities(10).unwrap().is_empty());
    }

    #[test]
    fn recording_the_same_prepared_transaction_converges() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let claim = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();
        let first = store
            .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
            .unwrap();

        let second = store
            .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
            .unwrap();

        assert_eq!(second.id, first.id);
        assert_eq!(second.txid, first.txid);
        assert_eq!(second.status, "prepared");
    }

    #[test]
    fn recording_prepared_bytes_backfills_a_legacy_broadcast() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let claim = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();
        let prepared = store
            .record_prepared(&claim.id, "real-txid", b"signed transaction", 140)
            .unwrap();
        let broadcast = store.mark_broadcast(&prepared.id, &prepared.txid).unwrap();
        store
            .0
            .lock()
            .unwrap()
            .execute("DELETE FROM prepared_payments", [])
            .unwrap();

        store
            .record_prepared(
                &broadcast.id,
                &broadcast.txid,
                b"recovered transaction",
                140,
            )
            .unwrap();

        assert_eq!(
            store
                .prepared_transaction(&broadcast.id)
                .unwrap()
                .raw_transaction,
            b"recovered transaction"
        );
    }

    #[test]
    fn stale_retry_does_not_reset_a_replacement_transaction() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let claim = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();
        store
            .record_prepared(&claim.id, "old-txid", b"old transaction", 140)
            .unwrap();
        store.reset_for_retry(&claim.id, "old-txid").unwrap();
        store
            .record_prepared(&claim.id, "new-txid", b"new transaction", 180)
            .unwrap();

        let current = store.reset_for_retry(&claim.id, "old-txid").unwrap();

        assert_eq!(current.txid, "new-txid");
        assert_eq!(current.status, "prepared");
    }

    #[test]
    fn broadcast_reconciliation_preserves_a_concurrent_confirmation() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let claim = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();
        store
            .record_prepared(&claim.id, "real-txid", b"transaction", 140)
            .unwrap();
        store
            .confirm(&claim.id, "real-txid", "mined-block")
            .unwrap();

        let current = store.mark_broadcast(&claim.id, "real-txid").unwrap();

        assert_eq!(current.status, "confirmed");
        assert_eq!(current.block_hash.as_deref(), Some("mined-block"));
    }

    #[test]
    fn prepared_transaction_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.db");
        let store = Store::open(&path).unwrap();
        store.initialize().unwrap();
        let claim = store
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();
        let id = claim.id.clone();
        store
            .record_prepared(&id, "real-txid", b"raw transaction", 140)
            .unwrap();
        drop(store);

        let reopened = Store::open(path).unwrap();
        reopened.initialize().unwrap();
        let recovered = reopened
            .claim_transfer(1, 2, "ironwood", "ironwood", 12_000, "same", None)
            .unwrap();

        assert_eq!(recovered.txid, "real-txid");
        assert_eq!(recovered.status, "prepared");
        let prepared = reopened.prepared_transaction(&id).unwrap();
        assert_eq!(prepared.raw_transaction, b"raw transaction");
        assert_eq!(prepared.expiry_height, 140);
    }
}
