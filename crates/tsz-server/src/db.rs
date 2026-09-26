use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use bip39::Mnemonic;
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zcash_keys::{
    address::Address,
    keys::{Era, UnifiedAddressRequest, UnifiedFullViewingKey, UnifiedSpendingKey},
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unified_full_viewing_key: Option<String>,
    pub transparent_zatoshi: u64,
    pub orchard_zatoshi: u64,
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
        let stored_seed = db
            .query_row("SELECT value FROM metadata WHERE key='seed'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        let seed = if let Some(seed) = stored_seed {
            hex::decode(seed).context("invalid wallet seed")?
        } else {
            let mut entropy = [0u8; 32];
            rand::rng().fill_bytes(&mut entropy);
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
            let mut query = db.prepare("SELECT id,name,unified_address,transparent_address,transparent_zatoshi,orchard_zatoshi FROM accounts ORDER BY id")?;
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
        self.0.lock().unwrap().query_row("SELECT id,name,unified_address,transparent_address,transparent_zatoshi,orchard_zatoshi FROM accounts WHERE id=?1", [id], row_account).with_context(|| format!("account {id} does not exist"))
    }

    pub fn activities(&self, limit: u32) -> Result<Vec<Activity>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare("SELECT id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,block_hash,status,created_at FROM activity ORDER BY rowid DESC LIMIT ?1")?;
        Ok(query
            .query_map([limit.min(100)], row_activity)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn unconfirmed_activities(&self) -> Result<Vec<Activity>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare("SELECT id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,block_hash,status,created_at FROM activity WHERE status!='confirmed' ORDER BY rowid ASC")?;
        Ok(query
            .query_map([], row_activity)?
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
        if block_hash.is_empty() {
            bail!("block hash is required to confirm activity");
        }
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
        unified_full_viewing_key: None,
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
                     orchard_zatoshi INTEGER NOT NULL DEFAULT 0
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
                     (id,name,unified_address,transparent_address,transparent_zatoshi,orchard_zatoshi)
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
            assert_eq!(account.orchard_zatoshi, 456);
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
                    "orchard_zatoshi",
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
            .transfer(1, 2, "orchard", "orchard", 12_000, "send", "real-txid")
            .unwrap();
        assert_eq!(activity.txid, "real-txid");
        assert_eq!(store.account(1).unwrap().transparent_zatoshi, 0);
    }

    #[test]
    fn unconfirmed_activities_skips_confirmed_rows() {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let pending = store
            .transfer(
                1,
                2,
                "orchard",
                "orchard",
                12_000,
                "pending-key",
                "txid-pending",
            )
            .unwrap();
        let mined = store
            .transfer(
                1,
                3,
                "orchard",
                "orchard",
                13_000,
                "mined-key",
                "txid-mined",
            )
            .unwrap();
        store.confirm(&mined.id, &"c".repeat(64)).unwrap();

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
            .transfer(
                1,
                2,
                "orchard",
                "orchard",
                12_000,
                "empty-hash",
                "txid-empty",
            )
            .unwrap();
        assert!(store.confirm(&pending.id, "").is_err());
        let again = store
            .transfer(1, 2, "orchard", "orchard", 12_000, "empty-hash", "ignored")
            .unwrap();
        assert_eq!(again.id, pending.id);
        assert_eq!(again.status, "broadcast");
        assert_eq!(again.block_hash, None);
    }
}
