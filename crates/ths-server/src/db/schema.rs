use super::accounts::derived_addresses;
use super::address_faucet::UNKNOWN_CREATED_AT;
use super::*;

impl Store {
    pub fn initialize(&self) -> Result<()> {
        self.create_or_migrate().map(|_| ())
    }

    /// Initializes the store for `init`. A new instance records
    /// `nu7_activation_height`; an existing instance must already use it, because
    /// its chain and wallet were built under the schedule it was created with.
    pub fn initialize_instance(&self, nu7_activation_height: u32) -> Result<()> {
        let created = self.create_or_migrate()?;
        let db = self.0.lock().unwrap();
        let stored = stored_nu7_activation_height(&db)?;
        // Stores created before the height was configurable activated NU7 at height 1.
        let existing = stored.or((!created).then_some(DEFAULT_NU7_ACTIVATION_HEIGHT));
        if let Some(existing) = existing {
            if existing != nu7_activation_height {
                bail!(
                    "this instance was created with NU7 activation height {existing} and cannot \
                     change to {nu7_activation_height}; start a new instance to use another height"
                );
            }
            if stored.is_some() {
                return Ok(());
            }
        }
        db.execute(
            "INSERT INTO metadata(key, value) VALUES('nu7_activation_height', ?1)",
            [nu7_activation_height.to_string()],
        )?;
        Ok(())
    }

    /// The NU7 activation height this instance was created with. Stores created
    /// before the height was configurable record none and activated NU7 at 1.
    pub fn nu7_activation_height(&self) -> Result<u32> {
        let db = self.0.lock().unwrap();
        Ok(stored_nu7_activation_height(&db)?.unwrap_or(DEFAULT_NU7_ACTIVATION_HEIGHT))
    }

    /// Creates or upgrades every table and returns whether this call created the
    /// wallet seed, which is the moment a new instance comes into existence.
    fn create_or_migrate(&self) -> Result<bool> {
        let db = self.0.lock().unwrap();
        db.execute_batch(&format!(
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
            CREATE TABLE IF NOT EXISTS address_faucets (
                key TEXT PRIMARY KEY, id TEXT UNIQUE NOT NULL, address TEXT NOT NULL,
                amount_zatoshi INTEGER NOT NULL, txid TEXT NOT NULL DEFAULT '',
                block_hash TEXT, status TEXT NOT NULL DEFAULT 'preparing', last_error TEXT,
                created_at TEXT NOT NULL DEFAULT '{UNKNOWN_CREATED_AT}'
            );
        "#
        ))?;
        db.execute_batch(ACTIVITY_TABLE)?;
        add_missing_column(&db, "idempotency", "memo")?;
        add_missing_column(&db, "activity", "last_error")?;
        add_missing_column(&db, "address_faucets", "last_error")?;
        add_address_faucets_created_at(&db)?;
        migrate_activity_recipients(&db)?;
        db.execute_batch(ACTIVITY_ORDER_TABLE)?;
        // Existing journals already have an immutable insertion order. Preserve
        // it when first-seen ordering is introduced for an older database.
        db.execute(
            "INSERT OR IGNORE INTO activity_order(activity_id)
             SELECT id FROM activity ORDER BY rowid ASC",
            [],
        )?;
        // An external payment predating the `created_at` column (and so this
        // merged feed) is backfilled with an explicit negative sequence,
        // always older than any positive, autoincremented sequence already
        // assigned above or earlier on a prior startup. Ordering the two
        // statements would not be enough on its own: on an instance that
        // already ran before address faucets joined this table,
        // `activity_order` already holds rows, so a plain autoincremented
        // backfill would still give these older payments the newest
        // sequence numbers. A payment created after the column exists
        // always carries a real `created_at` and is excluded here, so it
        // gets a normal sequence the first time `/api/v1/activity` reads it,
        // same as any other new activity.
        db.execute(
            &format!(
                "INSERT OR IGNORE INTO activity_order(sequence,activity_id)
                 SELECT -ROW_NUMBER() OVER (ORDER BY rowid DESC), id FROM address_faucets
                 WHERE created_at = '{UNKNOWN_CREATED_AT}'"
            ),
            [],
        )?;
        let stored_seed = db
            .query_row("SELECT value FROM metadata WHERE key='seed'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        let created = stored_seed.is_none();
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
        Ok(created)
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

fn stored_nu7_activation_height(db: &Connection) -> Result<Option<u32>> {
    db.query_row(
        "SELECT value FROM metadata WHERE key='nu7_activation_height'",
        [],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|value| {
        value
            .parse()
            .with_context(|| format!("invalid stored NU7 activation height {value:?}"))
    })
    .transpose()
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

/// Rows created before this column existed predate per-row creation times.
/// SQLite refuses a non-constant `ADD COLUMN` default (`CURRENT_TIMESTAMP`)
/// once the table holds rows, so backfill the `UNKNOWN_CREATED_AT` sentinel
/// instead: the activity feed reports it as an unknown time rather than a
/// real one, and treats it as older than anything already sequenced (see
/// the `activity_order` backfill below). Every insert going forward
/// supplies a real `created_at` explicitly, so this default otherwise only
/// matters for this one-time backfill.
fn add_address_faucets_created_at(db: &Connection) -> Result<()> {
    let exists = db
        .prepare("PRAGMA table_info(address_faucets)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|existing| existing == "created_at");
    if !exists {
        db.execute(
            &format!(
                "ALTER TABLE address_faucets ADD COLUMN created_at TEXT NOT NULL DEFAULT '{UNKNOWN_CREATED_AT}'"
            ),
            [],
        )?;
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

const ACTIVITY_ORDER_TABLE: &str = r#"
    CREATE TABLE IF NOT EXISTS activity_order (
        sequence INTEGER PRIMARY KEY AUTOINCREMENT,
        activity_id TEXT NOT NULL UNIQUE
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
