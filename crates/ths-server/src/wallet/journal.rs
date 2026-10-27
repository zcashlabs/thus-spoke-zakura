use super::*;
use std::collections::HashMap;

const PREPARED_PAYMENTS_MIGRATION_ID: Uuid =
    Uuid::from_u128(0x695f93ac_6935_47e8_8f06_017b1d7ec3aa);

// Keep the ext_tsz_* schema names and migration UUIDs stable: existing wallets
// have already applied these migrations, so changing their definitions would
// produce a different schema only for newly initialized wallets.
pub(super) struct PreparedPaymentsMigration;

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

pub(super) fn prepared_payment(db: &mut Db, activity_id: &str) -> Result<Option<PreparedPayment>> {
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

pub(super) fn prepared_payment_txids(
    db: &mut Db,
    activity_ids: &[String],
) -> Result<HashMap<String, String>> {
    db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
        activity_ids
            .iter()
            .filter_map(|activity_id| {
                match ext
                    .query_row(
                        "SELECT txid FROM ext_tsz_prepared_payments WHERE activity_id=?1",
                        [activity_id],
                        |row| row.get(0),
                    )
                    .optional()
                {
                    Ok(Some(txid)) => Some(Ok((activity_id.clone(), txid))),
                    Ok(None) => None,
                    Err(error) => Some(Err(error.into())),
                }
            })
            .collect()
    })
}
pub struct PreparedPayment {
    pub txid: String,
    pub raw_transaction: Vec<u8>,
    pub expiry_height: u64,
}

impl RealWallet {
    #[cfg(test)]
    pub async fn has_prepared(&self, activity_id: &str) -> Result<bool> {
        let activity_id = activity_id.to_owned();
        self.with_db(move |db| Ok(prepared_payment(db, &activity_id)?.is_some()))
            .await
    }

    pub async fn prepared(&self, activity_id: &str) -> Result<Option<PreparedPayment>> {
        let activity_id = activity_id.to_owned();
        self.with_db(move |db| prepared_payment(db, &activity_id))
            .await
    }

    pub async fn recover_prepared(&self, txid: &str) -> Result<Option<PreparedPayment>> {
        let txid = TxId::from_hex(txid).context("invalid wallet transaction id")?;
        self.with_db(move |db| {
            let Some(transaction) = db.get_transaction(txid)? else {
                return Ok(None);
            };
            Ok(Some(Self::serialize_transaction(txid, &transaction)?))
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn record_prepared_association_for_test(
        &self,
        activity_id: &str,
        prepared: PreparedPayment,
    ) -> Result<()> {
        let activity_id = activity_id.to_owned();
        self.with_db(move |db| {
            db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
                ext.execute(
                    "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height)
                     VALUES(?1,?2,?3,?4)",
                    rusqlite::params![
                        activity_id,
                        prepared.txid,
                        prepared.raw_transaction,
                        prepared.expiry_height
                    ],
                )?;
                Ok(())
            })
        })
        .await
    }
}

#[cfg(test)]
#[path = "../../tests/unit/wallet/journal/tests.rs"]
mod tests;

impl RealWallet {
    pub(crate) async fn prepared_shielding(&self) -> Result<Option<PreparedPayment>> {
        self.prepared(TREASURY_SHIELDING_JOURNAL_ID).await
    }
    pub(crate) async fn retire_shielding(&self, txid: &str) -> Result<()> {
        let txid = txid.to_owned();
        self.with_db(move |db| {
            db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
                let retired = ext.execute(
                    "DELETE FROM ext_tsz_prepared_payments WHERE activity_id=?1 AND txid=?2",
                    rusqlite::params![TREASURY_SHIELDING_JOURNAL_ID, txid],
                )?;
                anyhow::ensure!(retired == 1, "shielding journal changed before retirement");
                Ok(())
            })
        })
        .await
    }
}
