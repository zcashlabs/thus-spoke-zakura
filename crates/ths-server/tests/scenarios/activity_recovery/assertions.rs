use super::{
    Activity, Client, Connection, ConstructedTransactionRecord, OpenFlags, OptionalExtension,
    PathBuf, PendingPaymentSnapshot, PreparedPaymentJournalRecord, PreparedPaymentRecord,
    RECOVERY_IDEMPOTENCY_KEY, RegtestStack, Result, TransactionEvidence, json, params, rpc,
};

pub(super) fn pending_payment_snapshot(
    fixture: &RegtestStack,
    external: bool,
    key: &str,
) -> Result<PendingPaymentSnapshot> {
    let db = Connection::open_with_flags(
        fixture.data_dir().join("ths.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let sql = if external {
        "SELECT id,txid,status,block_hash FROM address_faucets WHERE key=?1"
    } else {
        "SELECT a.id,a.txid,a.status,a.block_hash FROM activity a JOIN idempotency i ON i.activity_id=a.id WHERE i.key=?1"
    };
    let (id, txid, status, block_hash): (String, String, String, Option<String>) =
        db.query_row(sql, [key], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
    anyhow::ensure!(
        status == "broadcast" && block_hash.is_none(),
        "payment is not pending without a block hash"
    );
    let (raw, expiry_height): (Vec<u8>, u64) = db.query_row(
        "SELECT raw_transaction,expiry_height FROM prepared_payments WHERE activity_id=?1",
        [&id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let wallet = Connection::open_with_flags(
        fixture.data_dir().join("wallet.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let (wallet_txid, wallet_raw, wallet_expiry): (String, Vec<u8>, u64) = wallet.query_row("SELECT txid,raw_transaction,expiry_height FROM ext_tsz_prepared_payments WHERE activity_id=?1", [&id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
    anyhow::ensure!(
        wallet_txid == txid && wallet_raw == raw && wallet_expiry == expiry_height,
        "wallet and server prepared payment journals differ"
    );
    // Compare reservation associations by transaction bytes, avoiding display-endian txid conversion.
    let mut statement = wallet.prepare(
        "SELECT 'ironwood',s.ironwood_received_note_id FROM ironwood_received_note_spends s JOIN transactions t ON t.id_tx=s.transaction_id WHERE t.raw=?1
         UNION ALL SELECT 'transparent',s.transparent_received_output_id FROM transparent_received_output_spends s JOIN transactions t ON t.id_tx=s.transaction_id WHERE t.raw=?1 ORDER BY 1,2")?;
    let reserved_inputs = statement
        .query_map([&raw], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    anyhow::ensure!(
        !reserved_inputs.is_empty(),
        "original transaction has no reserved wallet inputs"
    );
    Ok(PendingPaymentSnapshot {
        id,
        txid,
        raw,
        expiry_height,
        reserved_inputs,
    })
}

pub(super) fn constructed_transactions(
    path: &std::path::Path,
) -> Result<Vec<ConstructedTransactionRecord>> {
    let database = Connection::open(path.join("wallet.db"))?;
    let mut statement = database.prepare(
        "SELECT txid,raw,expiry_height FROM transactions
         WHERE created IS NOT NULL AND raw IS NOT NULL ORDER BY txid",
    )?;
    Ok(statement
        .query_map([], |row| {
            Ok(ConstructedTransactionRecord {
                txid: row.get(0)?,
                raw_transaction: row.get(1)?,
                expiry_height: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(super) fn prepared_payment_journal(
    path: &std::path::Path,
) -> Result<Vec<PreparedPaymentJournalRecord>> {
    let database = Connection::open(path.join("wallet.db"))?;
    let mut statement = database.prepare(
        "SELECT activity_id,txid,raw_transaction,expiry_height
         FROM ext_tsz_prepared_payments ORDER BY activity_id",
    )?;
    Ok(statement
        .query_map([], |row| {
            Ok(PreparedPaymentJournalRecord {
                activity_id: row.get(0)?,
                payment: PreparedPaymentRecord {
                    txid: row.get(1)?,
                    raw_transaction: row.get(2)?,
                    expiry_height: row.get(3)?,
                },
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(super) fn prepared_payment(
    path: &std::path::Path,
    activity_id: &str,
) -> Result<Option<PreparedPaymentRecord>> {
    Ok(Connection::open(path.join("wallet.db"))?
        .query_row(
            "SELECT txid,raw_transaction,expiry_height
             FROM ext_tsz_prepared_payments WHERE activity_id=?1",
            [activity_id],
            |row| {
                Ok(PreparedPaymentRecord {
                    txid: row.get(0)?,
                    raw_transaction: row.get(1)?,
                    expiry_height: row.get(2)?,
                })
            },
        )
        .optional()?)
}

pub(super) async fn assert_address_faucet_inclusion(
    client: &Client,
    fixture: &RegtestStack,
    response: &serde_json::Value,
    address: &str,
    key: &str,
) -> Result<()> {
    anyhow::ensure!(
        response.as_object().is_some_and(|fields| fields.len() == 6),
        "address faucet response fields changed"
    );
    anyhow::ensure!(
        response["address"] == address
            && response["amount_zatoshi"] == 100_000_000
            && response["status"] == "confirmed"
            && response["last_error"].is_null(),
        "address faucet destination, amount, or confirmation status changed"
    );
    let stored: serde_json::Value = request_json(
        client,
        fixture.api_url(),
        &format!("/api/v1/faucet/address?idempotency_key={key}"),
        None,
        API_READ_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(
        &stored == response,
        "stored address faucet status differs from the response"
    );
    let evidence: TransactionEvidence = rpc(
        client,
        fixture.node_url(),
        "getrawtransaction",
        json!([response["txid"], 1]),
    )
    .await?;
    anyhow::ensure!(
        evidence.confirmations.is_some_and(|count| count > 0),
        "faucet transaction is not confirmed"
    );
    anyhow::ensure!(
        evidence.blockhash.as_deref() == response["block_hash"].as_str(),
        "faucet inclusion hash differs from response"
    );
    Ok(())
}

pub(super) fn assert_requested_payment_fields(activity: &Activity) -> Result<()> {
    anyhow::ensure!(activity.kind == "send", "activity kind was not send");
    anyhow::ensure!(
        activity.from_account == Some(1),
        "activity source account changed"
    );
    anyhow::ensure!(
        activity.to_account == Some(2),
        "activity destination account changed"
    );
    anyhow::ensure!(
        activity.source_pool == "ironwood",
        "activity source pool changed"
    );
    anyhow::ensure!(
        activity.destination_pool == "ironwood",
        "activity destination pool changed"
    );
    anyhow::ensure!(
        activity.amount_zatoshi == 1_000_000,
        "activity amount changed"
    );
    anyhow::ensure!(!activity.id.is_empty(), "activity ID was empty");
    anyhow::ensure!(
        !activity.txid.is_empty(),
        "activity transaction ID was empty"
    );
    Ok(())
}

pub(super) fn one_matching_activity(
    activities: &[Activity],
    expected: &Activity,
) -> Result<Activity> {
    let id_matches = activities
        .iter()
        .filter(|activity| activity.id == expected.id)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        id_matches.len() == 1,
        "activity read did not contain exactly one matching ID"
    );
    anyhow::ensure!(
        id_matches[0].txid == expected.txid,
        "activity ID was not paired with the expected transaction ID"
    );
    let txid_matches = activities
        .iter()
        .filter(|activity| activity.txid == expected.txid)
        .count();
    anyhow::ensure!(
        txid_matches == 1,
        "activity read did not contain exactly one matching transaction ID"
    );
    Ok(id_matches[0].clone())
}

pub(super) fn assert_persisted_send_response(
    immediate: &Activity,
    persisted: &Activity,
) -> Result<()> {
    anyhow::ensure!(
        !persisted.created_at.is_empty(),
        "persisted activity timestamp was empty"
    );
    let mut expected = immediate.clone();
    expected.created_at = persisted.created_at.clone();
    anyhow::ensure!(
        persisted == &expected,
        "persisted activity changed beyond its generated timestamp: expected {expected:?}, got {persisted:?}"
    );
    Ok(())
}

pub(super) fn assert_read_only_persistence(
    database_path: PathBuf,
    persisted: &Activity,
    expected_block_hash: &str,
) -> Result<()> {
    let database = Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let activity_rows = {
        let mut statement = database.prepare(
            "SELECT id, txid, status, block_hash, created_at FROM activity WHERE txid = ?1",
        )?;
        statement
            .query_map(params![&persisted.txid], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    anyhow::ensure!(
        activity_rows.len() == 1,
        "read-only database query did not return exactly one activity"
    );
    let (id, txid, status, block_hash, created_at) = &activity_rows[0];
    anyhow::ensure!(id == &persisted.id, "persisted activity ID changed");
    anyhow::ensure!(txid == &persisted.txid, "persisted transaction ID changed");
    anyhow::ensure!(
        status == "confirmed",
        "persisted activity was not confirmed"
    );
    anyhow::ensure!(
        block_hash.as_deref() == Some(expected_block_hash),
        "persisted activity did not retain the transaction inclusion block hash"
    );
    anyhow::ensure!(
        created_at == &persisted.created_at,
        "persisted activity timestamp changed during recovery"
    );

    let mappings = {
        let mut statement =
            database.prepare("SELECT activity_id FROM idempotency WHERE key = ?1")?;
        statement
            .query_map(params![RECOVERY_IDEMPOTENCY_KEY], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    anyhow::ensure!(
        mappings.len() == 1 && mappings[0].as_str() == persisted.id,
        "read-only database query did not retain exactly one idempotency mapping"
    );
    Ok(())
}

pub(super) fn preserve_scenario_failure<T>(scenario: Result<T>, cleanup: Result<()>) -> Result<T> {
    match (scenario, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), _) => Err(error),
    }
}

use super::{API_READ_TIMEOUT, request_json};

pub(super) fn server_prepared_journal(
    path: &std::path::Path,
) -> Result<Vec<(String, Vec<u8>, u64)>> {
    let db = Connection::open_with_flags(path.join("ths.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut query = db.prepare("SELECT activity_id,raw_transaction,expiry_height FROM prepared_payments ORDER BY activity_id")?;
    Ok(query
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}
