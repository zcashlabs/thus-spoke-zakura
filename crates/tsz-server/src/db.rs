use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zcash_keys::{
    address::Address,
    keys::{UnifiedAddressRequest, UnifiedSpendingKey},
};
use zcash_protocol::{consensus::BlockHeight, local_consensus::LocalNetwork};

pub const ZATOSHIS_PER_ZEC: u64 = 100_000_000;
pub const USER_ACCOUNT_COUNT: u8 = 5;
pub const TREASURY_ACCOUNT_ID: u8 = 6;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Account {
    pub id: u8,
    pub name: String,
    pub unified_address: String,
    pub transparent_address: String,
    pub transparent_zatoshi: u64,
    pub orchard_zatoshi: u64,
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
                orchard_zatoshi INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS activity (
                id TEXT PRIMARY KEY, kind TEXT NOT NULL, from_account INTEGER, to_account INTEGER NOT NULL,
                source_pool TEXT NOT NULL, destination_pool TEXT NOT NULL, amount_zatoshi INTEGER NOT NULL,
                txid TEXT NOT NULL, block_hash TEXT, status TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS idempotency (key TEXT PRIMARY KEY, activity_id TEXT NOT NULL);
        "#)?;
        let seed = db
            .query_row("SELECT value FROM metadata WHERE key='seed'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        let entropy = if let Some(seed) = seed {
            hex::decode(seed).context("invalid wallet seed")?
        } else {
            let mut entropy = [0u8; 32];
            rand::rng().fill_bytes(&mut entropy);
            db.execute(
                "INSERT INTO metadata(key, value) VALUES('seed', ?1)",
                [hex::encode(entropy)],
            )?;
            entropy.to_vec()
        };
        for id in 1u8..=TREASURY_ACCOUNT_ID {
            let (ua, taddr) = derived_addresses(&entropy, id)?;
            db.execute("INSERT OR IGNORE INTO accounts(id,name,unified_address,transparent_address) VALUES(?1,?2,?3,?4)", params![id, format!("Account {id}"), ua, taddr])?;
        }
        Ok(())
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare("SELECT id,name,unified_address,transparent_address,transparent_zatoshi,orchard_zatoshi FROM accounts ORDER BY id")?;
        Ok(query
            .query_map([], row_account)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn user_accounts(&self) -> Result<Vec<Account>> {
        Ok(self
            .accounts()?
            .into_iter()
            .filter(|account| account.id <= USER_ACCOUNT_COUNT)
            .collect())
    }

    pub fn account(&self, id: u8) -> Result<Account> {
        self.0.lock().unwrap().query_row("SELECT id,name,unified_address,transparent_address,transparent_zatoshi,orchard_zatoshi FROM accounts WHERE id=?1", [id], row_account).with_context(|| format!("account {id} does not exist"))
    }

    pub fn activities(&self, limit: u32) -> Result<Vec<Activity>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare("SELECT id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,block_hash,status,created_at FROM activity ORDER BY rowid DESC LIMIT ?1")?;
        Ok(query
            .query_map([limit.min(100)], row_activity)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn activity_for_key(&self, key: &str) -> Result<Option<Activity>> {
        let db = self.0.lock().unwrap();
        db.query_row(
            "SELECT a.id,a.kind,a.from_account,a.to_account,a.source_pool,a.destination_pool,a.amount_zatoshi,a.txid,a.block_hash,a.status,a.created_at FROM activity a JOIN idempotency i ON i.activity_id=a.id WHERE i.key=?1",
            [key], row_activity,
        ).optional().map_err(Into::into)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn transfer(
        &self,
        from: u8,
        to: u8,
        source_pool: &str,
        destination_pool: &str,
        amount: u64,
        key: &str,
        txid: &str,
    ) -> Result<Activity> {
        validate_pool(source_pool)?;
        validate_pool(destination_pool)?;
        if amount == 0 {
            bail!("amount must be greater than zero");
        }
        let mut db = self.0.lock().unwrap();
        if let Some(id) = db
            .query_row(
                "SELECT activity_id FROM idempotency WHERE key=?1",
                [key],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            return db.query_row("SELECT id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,block_hash,status,created_at FROM activity WHERE id=?1", [id], row_activity).map_err(Into::into);
        }
        for id in [from, to] {
            if !db.query_row(
                "SELECT EXISTS(SELECT 1 FROM accounts WHERE id=?1)",
                [id],
                |r| r.get::<_, bool>(0),
            )? {
                bail!("account {id} does not exist");
            }
        }
        let tx = db.transaction()?;
        let mut activity = new_activity(
            "send",
            Some(from),
            to,
            source_pool,
            destination_pool,
            amount,
        );
        activity.txid = txid.to_owned();
        insert_activity(&tx, &activity, key)?;
        tx.commit()?;
        Ok(activity)
    }

    pub fn faucet(
        &self,
        to: u8,
        pool: &str,
        amount: u64,
        key: &str,
        txid: &str,
    ) -> Result<Activity> {
        validate_pool(pool)?;
        if amount == 0 {
            bail!("amount must be greater than zero");
        }
        let mut db = self.0.lock().unwrap();
        if let Some(id) = db
            .query_row(
                "SELECT activity_id FROM idempotency WHERE key=?1",
                [key],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            return db.query_row("SELECT id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,block_hash,status,created_at FROM activity WHERE id=?1", [id], row_activity).map_err(Into::into);
        }
        if !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM accounts WHERE id=?1)",
            [to],
            |r| r.get::<_, bool>(0),
        )? {
            bail!("account {to} does not exist");
        }
        let tx = db.transaction()?;
        let mut activity = new_activity("faucet", None, to, "orchard", pool, amount);
        activity.txid = txid.to_owned();
        insert_activity(&tx, &activity, key)?;
        tx.commit()?;
        Ok(activity)
    }

    pub fn confirm(&self, id: &str, block_hash: &str) -> Result<Activity> {
        let db = self.0.lock().unwrap();
        db.execute(
            "UPDATE activity SET status='confirmed',block_hash=?1 WHERE id=?2",
            params![block_hash, id],
        )?;
        db.query_row("SELECT id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,block_hash,status,created_at FROM activity WHERE id=?1", [id], row_activity).map_err(Into::into)
    }

    pub fn seed(&self) -> Result<String> {
        Ok(self.0.lock().unwrap().query_row(
            "SELECT value FROM metadata WHERE key='seed'",
            [],
            |r| r.get(0),
        )?)
    }
}

fn insert_activity(db: &Connection, a: &Activity, key: &str) -> Result<()> {
    db.execute("INSERT INTO activity(id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,status) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![a.id,a.kind,a.from_account,a.to_account,a.source_pool,a.destination_pool,a.amount_zatoshi,a.txid,a.status])?;
    db.execute(
        "INSERT INTO idempotency(key,activity_id) VALUES(?1,?2)",
        params![key, a.id],
    )?;
    Ok(())
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
    let txid = hex::encode(Sha256::digest(id.as_bytes()));
    Activity {
        id,
        kind: kind.into(),
        from_account: from,
        to_account: to,
        source_pool: source.into(),
        destination_pool: destination.into(),
        amount_zatoshi: amount,
        txid,
        block_hash: None,
        status: "broadcast".into(),
        created_at: String::new(),
    }
}
fn validate_pool(pool: &str) -> Result<()> {
    if matches!(pool, "transparent" | "orchard") {
        Ok(())
    } else {
        bail!("pool must be transparent or orchard")
    }
}
fn row_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: row.get(0)?,
        name: row.get(1)?,
        unified_address: row.get(2)?,
        transparent_address: row.get(3)?,
        transparent_zatoshi: row.get(4)?,
        orchard_zatoshi: row.get(5)?,
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
fn derived_addresses(seed: &[u8], id: u8) -> Result<(String, String)> {
    let one = Some(BlockHeight::from_u32(1));
    let network = LocalNetwork {
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
    };
    let account = zip32::AccountId::try_from(u32::from(id - 1))
        .map_err(|_| anyhow::anyhow!("invalid ZIP-32 account {id}"))?;
    let usk = UnifiedSpendingKey::from_seed(&network, seed, account)
        .map_err(|error| anyhow::anyhow!("deriving account {id}: {error:?}"))?;
    let (ua, _) = usk
        .to_unified_full_viewing_key()
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

#[cfg(test)]
mod tests {
    use super::*;
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
            .faucet(2, "orchard", ZATOSHIS_PER_ZEC, "same", "txid")
            .unwrap();
        let second = store
            .faucet(2, "orchard", ZATOSHIS_PER_ZEC, "same", "ignored")
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(store.account(2).unwrap().orchard_zatoshi, 0);
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
    fn records_real_transfer_without_mutating_balances() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let activity = store
            .transfer(1, 2, "orchard", "orchard", 12_000, "send", "real-txid")
            .unwrap();
        assert_eq!(activity.txid, "real-txid");
        assert_eq!(store.account(1).unwrap().transparent_zatoshi, 0);
    }
}
