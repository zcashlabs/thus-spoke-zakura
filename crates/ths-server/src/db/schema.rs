use super::accounts::derived_addresses;
use super::*;

impl Store {
    pub fn initialize(&self) -> Result<()> {
        let db = self.0.lock().unwrap();
        db.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS accounts (
                id INTEGER PRIMARY KEY, name TEXT NOT NULL, unified_address TEXT NOT NULL,
                transparent_address TEXT NOT NULL, transparent_zatoshi INTEGER NOT NULL DEFAULT 0,
                ironwood_zatoshi INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS idempotency (
                key TEXT PRIMARY KEY, activity_id TEXT NOT NULL, memo TEXT
            );
            CREATE TABLE IF NOT EXISTS prepared_payments (
                activity_id TEXT PRIMARY KEY, raw_transaction BLOB NOT NULL,
                expiry_height INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS activity_outputs (
                activity_id TEXT NOT NULL, position INTEGER NOT NULL, to_account INTEGER,
                to_address TEXT, destination_pool TEXT NOT NULL,
                amount_zatoshi INTEGER NOT NULL, memo TEXT,
                PRIMARY KEY (activity_id, position)
            );
            CREATE TABLE IF NOT EXISTS address_faucets (
                key TEXT PRIMARY KEY, id TEXT UNIQUE NOT NULL, address TEXT NOT NULL,
                amount_zatoshi INTEGER NOT NULL, txid TEXT NOT NULL DEFAULT '',
                block_hash TEXT, status TEXT NOT NULL DEFAULT 'preparing', last_error TEXT
            );
        "#,
        )?;
        db.execute_batch(ACTIVITY_TABLE)?;
        add_missing_column(&db, "idempotency", "memo")?;
        add_missing_column(&db, "activity", "last_error")?;
        add_missing_column(&db, "address_faucets", "last_error")?;
        migrate_activity_recipients(&db)?;
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
}

fn add_missing_column(db: &Connection, table: &str, column: &str) -> Result<()> {
    let exists = db
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|existing| existing == column);
    if !exists {
        db.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} TEXT"), [])?;
    }
    Ok(())
}

const ACTIVITY_TABLE: &str = r#"
    CREATE TABLE IF NOT EXISTS activity (
        id TEXT PRIMARY KEY, kind TEXT NOT NULL, from_account INTEGER, to_account INTEGER,
        to_address TEXT, source_pool TEXT NOT NULL, destination_pool TEXT NOT NULL,
        amount_zatoshi INTEGER NOT NULL, txid TEXT NOT NULL, block_hash TEXT, status TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, last_error TEXT
    );
"#;

/// Databases created before address sends have `to_account NOT NULL` and no
/// `to_address`. SQLite cannot relax a constraint in place, so rebuild the
/// table, keeping rowid order because activity is listed by rowid.
fn migrate_activity_recipients(db: &Connection) -> Result<()> {
    let migrated: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('activity') WHERE name='to_address')",
        [],
        |r| r.get(0),
    )?;
    if migrated {
        return Ok(());
    }
    let tx = db.unchecked_transaction()?;
    tx.execute_batch("ALTER TABLE activity RENAME TO activity_before_recipients;")?;
    tx.execute_batch(ACTIVITY_TABLE)?;
    tx.execute_batch(
        "INSERT INTO activity(id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,block_hash,status,created_at,last_error)
         SELECT id,kind,from_account,to_account,source_pool,destination_pool,amount_zatoshi,txid,block_hash,status,created_at,last_error
         FROM activity_before_recipients ORDER BY rowid;
         DROP TABLE activity_before_recipients;",
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/db/schema/tests.rs"]
mod tests;
