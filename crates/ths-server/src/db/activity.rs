use super::address_faucet::address_for_key;
use super::*;

/// Stored as a batch send's `destination_pool` when its outputs span pools.
/// Each output keeps its own pool in `activity_outputs`.
pub(crate) const MIXED_POOLS: &str = "mixed";

/// Qualified so it also reads unambiguously in joins. The order must match `row_activity`.
const ACTIVITY_COLUMNS: &str = "a.id,a.kind,a.from_account,a.to_account,a.to_address,a.source_pool,a.destination_pool,a.amount_zatoshi,a.txid,a.block_hash,a.status,a.created_at,a.last_error";
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Activity {
    pub id: String,
    pub kind: String,
    pub from_account: Option<u8>,
    pub to_account: Option<u8>,
    pub to_address: Option<String>,
    pub source_pool: String,
    #[serde(serialize_with = "serialize_destination_pool")]
    pub destination_pool: String,
    pub amount_zatoshi: u64,
    pub txid: String,
    pub block_hash: Option<String>,
    pub status: String,
    pub created_at: String,
    pub last_error: Option<String>,
    /// Empty for a single-recipient payment, whose recipient is on the row.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<ActivityOutput>,
}

/// A batch spanning pools has no single destination pool; its outputs name each.
fn serialize_destination_pool<S: serde::Serializer>(
    pool: &str,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    if pool == MIXED_POOLS {
        serializer.serialize_none()
    } else {
        serializer.serialize_some(pool)
    }
}

/// One payment of a multi-recipient send. Field order defines the canonical
/// sort, so equal payment sets compare equal however a URI ordered them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ActivityOutput {
    pub to_account: Option<u8>,
    pub to_address: Option<String>,
    pub destination_pool: String,
    pub amount_zatoshi: u64,
    pub memo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedTransaction {
    pub raw_transaction: Vec<u8>,
    pub expiry_height: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipient<'a> {
    Account(u8),
    Address(&'a str),
}

impl Store {
    pub(crate) fn unconfirm_blocks(&self, hashes: &[String]) -> Result<Vec<(String, String)>> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        let mut confirmations = Vec::new();
        for hash in hashes {
            for table in ["activity", "address_faucets"] {
                let mut query = tx.prepare(&format!(
                    "SELECT txid,block_hash FROM {table} WHERE status='confirmed' AND block_hash=?1"
                ))?;
                confirmations.extend(
                    query
                        .query_map([hash], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                );
                tx.execute(
                    &format!("UPDATE {table} SET status='unconfirmed',block_hash=NULL,last_error=NULL WHERE status='confirmed' AND block_hash=?1"),
                    [hash],
                )?;
            }
        }
        tx.commit()?;
        Ok(confirmations)
    }

    pub(crate) fn confirm_reorged_transaction(&self, txid: &str, hash: &str) -> Result<()> {
        if hash.is_empty() {
            bail!("block hash is required to confirm activity");
        }
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        for table in ["activity", "address_faucets"] {
            tx.execute(
                &format!("UPDATE {table} SET status='confirmed',block_hash=?1,last_error=NULL WHERE status='unconfirmed' AND txid=?2"),
                params![hash, txid],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn activities(&self, limit: u32) -> Result<Vec<Activity>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(&format!(
            "SELECT {ACTIVITY_COLUMNS} FROM activity a ORDER BY a.rowid DESC LIMIT ?1"
        ))?;
        Ok(query
            .query_map([limit.min(100)], |row| {
                with_outputs(&db, row_activity(row)?)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    #[cfg(test)]
    pub fn unconfirmed_activities(&self) -> Result<Vec<Activity>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(&format!(
            "SELECT {ACTIVITY_COLUMNS} FROM activity a WHERE a.txid!='' AND a.status NOT IN ('confirmed','failed') ORDER BY a.rowid ASC"
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
        to: Recipient<'_>,
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
            Some(to),
            source_pool,
            destination_pool,
            amount,
        );
        self.claim(request, key, memo, &[])
    }

    /// Claims one send that pays every output in a single transaction. The
    /// activity row carries the total; the outputs are stored with it so
    /// preparation and replays after a restart use exactly these recipients.
    pub fn claim_batch_transfer(
        &self,
        from: u8,
        source_pool: &str,
        outputs: &[ActivityOutput],
        key: &str,
    ) -> Result<Activity> {
        validate_pool(source_pool)?;
        if outputs.len() < 2 {
            bail!("a batch send needs at least two outputs");
        }
        let mut amount = 0u64;
        for output in outputs {
            validate_pool(&output.destination_pool)?;
            if output.amount_zatoshi == 0 {
                bail!("amount must be greater than zero");
            }
            if output.to_account.is_some() == output.to_address.is_some() {
                bail!("each output needs exactly one of an account or an address");
            }
            amount = amount
                .checked_add(output.amount_zatoshi)
                .context("batch amount overflows")?;
        }
        let first_pool = &outputs[0].destination_pool;
        let destination_pool = if outputs
            .iter()
            .all(|output| &output.destination_pool == first_pool)
        {
            first_pool.as_str()
        } else {
            MIXED_POOLS
        };
        let request = new_activity(
            "send",
            Some(from),
            None,
            source_pool,
            destination_pool,
            amount,
        );
        self.claim(request, key, None, outputs)
    }

    pub fn activity_outputs(&self, id: &str) -> Result<Vec<ActivityOutput>> {
        let db = self.0.lock().unwrap();
        activity_outputs(&db, id).map_err(Into::into)
    }

    pub fn claim_faucet(&self, to: u8, pool: &str, amount: u64, key: &str) -> Result<Activity> {
        validate_pool(pool)?;
        let request = new_activity(
            "faucet",
            None,
            Some(Recipient::Account(to)),
            "ironwood",
            pool,
            amount,
        );
        self.claim(request, key, None, &[])
    }

    /// Returns the payment already claimed with `key`, or records `request` as a new
    /// `preparing` claim. Submission and recovery happen outside this function.
    /// `outputs` is empty for a single-recipient payment.
    fn claim(
        &self,
        mut request: Activity,
        key: &str,
        memo: Option<&str>,
        outputs: &[ActivityOutput],
    ) -> Result<Activity> {
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
            if !same_payment(&activity, &request)
                || saved_memo.as_deref() != memo
                || activity.outputs != outputs
            {
                return Err(IdempotencyConflict.into());
            }
            return Ok(activity);
        }
        if address_for_key(&db, key)?.is_some() {
            return Err(IdempotencyConflict.into());
        }
        let output_accounts = outputs.iter().filter_map(|output| output.to_account);
        for id in request
            .from_account
            .into_iter()
            .chain(request.to_account)
            .chain(output_accounts)
        {
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
        for (position, output) in outputs.iter().enumerate() {
            tx.execute(
                "INSERT INTO activity_outputs(activity_id,position,to_account,to_address,destination_pool,amount_zatoshi,memo) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![request.id, position, output.to_account, output.to_address, output.destination_pool, output.amount_zatoshi, output.memo],
            )?;
        }
        tx.commit()?;
        request.outputs = outputs.to_vec();
        Ok(request)
    }

    #[cfg(test)]
    pub fn discard_preparing(&self, id: &str) -> Result<()> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        if tx.execute(
            "DELETE FROM activity WHERE id=?1 AND status='preparing'",
            [id],
        )? == 1
        {
            tx.execute("DELETE FROM idempotency WHERE activity_id=?1", [id])?;
            tx.execute("DELETE FROM activity_outputs WHERE activity_id=?1", [id])?;
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
            "UPDATE activity SET txid=?1,status='prepared',last_error=NULL WHERE id=?2 AND status='preparing'",
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

    #[cfg(test)]
    pub fn prepared_transaction(&self, id: &str) -> Result<PreparedTransaction> {
        self.optional_prepared_transaction(id)?
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
            "UPDATE activity SET status='broadcast',last_error=NULL WHERE id=?1 AND txid=?2 AND status IN ('prepared','broadcast')",
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
            "UPDATE activity SET status='confirmed',block_hash=?1,last_error=NULL WHERE id=?2 AND txid=?3",
            params![block_hash, id, txid],
        )?;
        activity_by_id(&db, id).map_err(Into::into)
    }

    pub fn record_error(
        &self,
        id: &str,
        status: &str,
        txid: &str,
        error: &str,
    ) -> Result<Activity> {
        let db = self.0.lock().unwrap();
        db.execute(
            "UPDATE activity SET last_error=?1 WHERE id=?2 AND status=?3 AND txid=?4 AND status NOT IN ('confirmed','failed')",
            params![error, id, status, txid],
        )?;
        activity_by_id(&db, id).map_err(Into::into)
    }

    #[cfg(test)]
    pub fn fail(&self, id: &str, error: &str) -> Result<Activity> {
        let db = self.0.lock().unwrap();
        let updated = db.execute(
            "UPDATE activity SET status='failed',last_error=?1 WHERE id=?2 AND status='preparing' AND txid=''",
            params![error, id],
        )?;
        let activity = activity_by_id(&db, id)?;
        if updated == 0 && activity.status != "failed" {
            bail!("payment {id} is no longer waiting for preparation");
        }
        Ok(activity)
    }

    /// A failed preparation is not terminal while an earlier retained operation
    /// can still provide the source funds. Check and transition under one lock
    /// so recovery cannot change a dependency between the check and the write.
    /// Deliberately orphaned payments are only observed, not executed, so they
    /// cannot defer an otherwise unfunded send while waiting for reconfirmation.
    pub(crate) fn fail_unfunded_send(&self, id: &str, error: &str) -> Result<Activity> {
        let db = self.0.lock().unwrap();
        db.execute(
            "UPDATE activity AS target SET status='failed',last_error=?1
             WHERE target.id=?2 AND target.kind='send' AND target.status='preparing' AND target.txid=''
             AND NOT EXISTS (
                 SELECT 1 FROM activity earlier
                 WHERE earlier.rowid < target.rowid
                   AND earlier.status IN ('preparing','prepared','broadcast')
                   AND (earlier.txid!='' OR EXISTS (SELECT 1 FROM idempotency i WHERE i.activity_id=earlier.id))
                   AND (
                       (earlier.destination_pool=target.source_pool AND (
                           earlier.to_account=target.from_account
                           OR earlier.to_address IN (
                               SELECT unified_address FROM accounts WHERE id=target.from_account
                               UNION ALL SELECT transparent_address FROM accounts WHERE id=target.from_account
                           )
                       ))
                       OR EXISTS (
                           SELECT 1 FROM activity_outputs o
                           WHERE o.activity_id=earlier.id AND o.destination_pool=target.source_pool
                             AND (o.to_account=target.from_account OR o.to_address IN (
                                 SELECT unified_address FROM accounts WHERE id=target.from_account
                                 UNION ALL SELECT transparent_address FROM accounts WHERE id=target.from_account
                             ))
                       )
                       OR (earlier.kind='send' AND earlier.from_account=target.from_account AND target.source_pool='ironwood')
                   )
             )",
            params![error, id],
        )?;
        activity_by_id(&db, id).map_err(Into::into)
    }
}

fn insert_activity(db: &Connection, a: &Activity, key: &str, memo: Option<&str>) -> Result<()> {
    db.execute("INSERT INTO activity(id,kind,from_account,to_account,to_address,source_pool,destination_pool,amount_zatoshi,txid,status) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)", params![a.id,a.kind,a.from_account,a.to_account,a.to_address,a.source_pool,a.destination_pool,a.amount_zatoshi,a.txid,a.status])?;
    db.execute(
        "INSERT INTO idempotency(key,activity_id,memo) VALUES(?1,?2,?3)",
        params![key, a.id, memo],
    )?;
    Ok(())
}

fn activity_outputs(db: &Connection, id: &str) -> rusqlite::Result<Vec<ActivityOutput>> {
    let mut query = db.prepare_cached(
        "SELECT to_account,to_address,destination_pool,amount_zatoshi,memo FROM activity_outputs WHERE activity_id=?1 ORDER BY position",
    )?;
    query
        .query_map([id], |row| {
            Ok(ActivityOutput {
                to_account: row.get(0)?,
                to_address: row.get(1)?,
                destination_pool: row.get(2)?,
                amount_zatoshi: row.get(3)?,
                memo: row.get(4)?,
            })
        })?
        .collect()
}

/// Rows carry no outputs; every read that returns an activity attaches them.
fn with_outputs(db: &Connection, mut activity: Activity) -> rusqlite::Result<Activity> {
    activity.outputs = activity_outputs(db, &activity.id)?;
    Ok(activity)
}

fn activity_by_id(db: &Connection, id: &str) -> rusqlite::Result<Activity> {
    let activity = db.query_row(
        &format!("SELECT {ACTIVITY_COLUMNS} FROM activity a WHERE a.id=?1"),
        [id],
        row_activity,
    )?;
    with_outputs(db, activity)
}

pub(super) fn activity_for_key(db: &Connection, key: &str) -> Result<Option<Activity>> {
    db.query_row(
        &format!(
            "SELECT {ACTIVITY_COLUMNS} FROM activity a JOIN idempotency i ON i.activity_id=a.id WHERE i.key=?1"
        ),
        [key],
        row_activity,
    )
    .optional()?
    .map(|activity| with_outputs(db, activity))
    .transpose()
    .map_err(Into::into)
}
fn new_activity(
    kind: &str,
    from: Option<u8>,
    // `None` for a batch send, whose recipients live in `activity_outputs`.
    to: Option<Recipient<'_>>,
    source: &str,
    destination: &str,
    amount: u64,
) -> Activity {
    let id = Uuid::new_v4().to_string();
    Activity {
        id,
        kind: kind.into(),
        from_account: from,
        to_account: match to {
            Some(Recipient::Account(id)) => Some(id),
            Some(Recipient::Address(_)) | None => None,
        },
        to_address: match to {
            Some(Recipient::Address(address)) => Some(address.to_owned()),
            Some(Recipient::Account(_)) | None => None,
        },
        source_pool: source.into(),
        destination_pool: destination.into(),
        amount_zatoshi: amount,
        txid: String::new(),
        block_hash: None,
        status: "preparing".into(),
        created_at: String::new(),
        last_error: None,
        outputs: Vec::new(),
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
        && existing.to_address == request.to_address
        && existing.source_pool == request.source_pool
        && existing.destination_pool == request.destination_pool
        && existing.amount_zatoshi == request.amount_zatoshi
}
fn row_activity(row: &rusqlite::Row<'_>) -> rusqlite::Result<Activity> {
    Ok(Activity {
        id: row.get(0)?,
        kind: row.get(1)?,
        from_account: row.get(2)?,
        to_account: row.get(3)?,
        to_address: row.get(4)?,
        source_pool: row.get(5)?,
        destination_pool: row.get(6)?,
        amount_zatoshi: row.get(7)?,
        txid: row.get(8)?,
        block_hash: row.get(9)?,
        status: row.get(10)?,
        created_at: row.get(11)?,
        last_error: row.get(12)?,
        outputs: Vec::new(),
    })
}

#[cfg(test)]
#[path = "../../tests/unit/db/activity/tests.rs"]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityIntent {
    pub activity: Activity,
    pub key: String,
    pub memo: Option<String>,
}

pub(crate) enum ActivityRecovery {
    Intent(ActivityIntent),
    Legacy(Activity),
}

impl ActivityRecovery {
    pub(crate) fn id(&self) -> &str {
        match self {
            Self::Intent(intent) => &intent.activity.id,
            Self::Legacy(activity) => &activity.id,
        }
    }
}

impl Store {
    /// Rowids keep admission order across timestamp ties and restarts. Keyed
    /// intents and legacy transactions share one queue; never reorder by UUID.
    pub(crate) fn pending_activities(&self) -> Result<Vec<(i64, ActivityRecovery)>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(&format!(
            "SELECT {ACTIVITY_COLUMNS},i.key,i.memo,a.rowid
             FROM activity a LEFT JOIN idempotency i ON i.activity_id=a.id
             WHERE a.status NOT IN ('confirmed','failed') AND (i.key IS NOT NULL OR a.txid!='')
             ORDER BY a.rowid ASC"
        ))?;
        Ok(query
            .query_map([], |row| {
                let activity = with_outputs(&db, row_activity(row)?)?;
                let key: Option<String> = row.get(13)?;
                let recovery = match key {
                    Some(key) => ActivityRecovery::Intent(ActivityIntent {
                        activity,
                        key,
                        memo: row.get(14)?,
                    }),
                    None => ActivityRecovery::Legacy(activity),
                };
                Ok((row.get(15)?, recovery))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    #[cfg(test)]
    pub fn unresolved_activities(&self) -> Result<Vec<ActivityIntent>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(&format!(
            "SELECT {ACTIVITY_COLUMNS},i.key,i.memo FROM activity a JOIN idempotency i ON i.activity_id=a.id WHERE a.status NOT IN ('confirmed','failed') ORDER BY a.rowid ASC"
        ))?;
        Ok(query
            .query_map([], |row| {
                Ok(ActivityIntent {
                    activity: row_activity(row)?,
                    key: row.get(13)?,
                    memo: row.get(14)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    #[cfg(test)]
    pub fn prepared_transaction_expiry(&self, id: &str) -> Result<Option<u64>> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT expiry_height FROM prepared_payments WHERE activity_id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }
}

impl Store {
    pub fn unconfirmed_legacy_activity(&self, id: &str) -> Result<Option<Activity>> {
        let db = self.0.lock().unwrap();
        db.query_row(
            &format!(
                "SELECT {ACTIVITY_COLUMNS} FROM activity a WHERE a.id=?1 AND a.txid!='' AND a.status NOT IN ('confirmed','failed') AND NOT EXISTS (SELECT 1 FROM idempotency i WHERE i.activity_id=a.id)"
            ),
            [id],
            row_activity,
        )
        .optional()
        .map_err(Into::into)
    }
    pub fn activity(&self, id: &str) -> Result<Activity> {
        let db = self.0.lock().unwrap();
        activity_by_id(&db, id).map_err(Into::into)
    }
    pub fn clear_error(
        &self,
        id: &str,
        expected_status: &str,
        expected_txid: &str,
    ) -> Result<Activity> {
        let db = self.0.lock().unwrap();
        db.execute(
            "UPDATE activity SET last_error=NULL WHERE id=?1 AND status=?2 AND txid=?3 AND status IN ('preparing','prepared','broadcast','unconfirmed')",
            params![id, expected_status, expected_txid],
        )?;
        activity_by_id(&db, id).map_err(Into::into)
    }
    pub fn optional_prepared_transaction(&self, id: &str) -> Result<Option<PreparedTransaction>> {
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
            .optional()
            .with_context(|| format!("reading prepared transaction {id}"))
    }
}
