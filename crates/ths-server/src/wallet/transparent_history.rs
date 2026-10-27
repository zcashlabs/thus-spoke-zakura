use super::*;

pub(super) struct TransparentHistoryMigration;

impl schemerz::Migration<Uuid> for TransparentHistoryMigration {
    fn id(&self) -> Uuid {
        Uuid::from_u128(0x87e5a89d_2ae6_4c60_996e_9d988b5e5984)
    }

    fn dependencies(&self) -> HashSet<Uuid> {
        migrations::V_0_22_0_RC2.iter().copied().collect()
    }

    fn description(&self) -> &'static str {
        "Public transparent transaction history coverage"
    }
}

impl RusqliteMigration for TransparentHistoryMigration {
    type Error = WalletMigrationError;

    fn up(&self, db: &rusqlite::Transaction<'_>) -> Result<(), Self::Error> {
        db.execute_batch(
            "CREATE TABLE ext_ths_transparent_history (
                account_uuid TEXT NOT NULL,
                address TEXT NOT NULL,
                height INTEGER NOT NULL CHECK(height >= 1),
                block_hash TEXT NOT NULL,
                PRIMARY KEY(account_uuid, address)
            );",
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HistoryQuery {
    pub account_id: AccountUuid,
    pub address: String,
    pub cursor: Option<ChainCheckpoint>,
}

pub(crate) struct HistoryTransaction {
    pub transaction: Transaction,
    pub height: u32,
}

impl HistoryTransaction {
    pub(crate) async fn decode(raw: RawTransaction, start: u32, end: u32) -> Result<Self> {
        tokio::task::spawn_blocking(move || {
            let height = u32::try_from(raw.height).context("invalid history transaction height")?;
            anyhow::ensure!(
                (start..=end).contains(&height) && height >= WALLET_BIRTHDAY_HEIGHT,
                "history transaction is outside the requested mined range"
            );
            let mut bytes = &raw.data[..];
            let transaction = Transaction::read(
                &mut bytes,
                BranchId::for_height(&regtest_network(), height.into()),
            )
            .context("parsing transparent history transaction")?;
            anyhow::ensure!(bytes.is_empty(), "trailing history transaction bytes");
            Ok(Self {
                transaction,
                height,
            })
        })
        .await
        .context("history transaction decoder stopped")?
    }
}

impl RealWallet {
    pub(crate) async fn transparent_history_queries(&self) -> Result<Vec<HistoryQuery>> {
        let queries = self.public_transparent_queries().await?;
        self.with_db(move |db| {
            db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
                let mut result = Vec::new();
                for query in queries {
                    for address in query.addresses {
                        let cursor = ext
                            .query_row(
                                "SELECT height,block_hash FROM ext_ths_transparent_history WHERE account_uuid=?1 AND address=?2",
                                rusqlite::params![query.account_id.expose_uuid().to_string(), address],
                                |row| Ok(ChainCheckpoint { height: row.get(0)?, hash: row.get(1)? }),
                            )
                            .optional()?;
                        result.push(HistoryQuery { account_id: query.account_id, address, cursor });
                    }
                }
                Ok(result)
            })
        }).await
    }

    pub(crate) async fn transparent_history_checkpoints(&self) -> Result<Vec<ChainCheckpoint>> {
        let mut checkpoints = self
            .transparent_history_queries()
            .await?
            .into_iter()
            .filter_map(|query| query.cursor)
            .collect::<Vec<_>>();
        checkpoints.sort_by(|a, b| (a.height, &a.hash).cmp(&(b.height, &b.hash)));
        checkpoints.dedup();
        Ok(checkpoints)
    }

    pub(crate) async fn import_transparent_history(
        &self,
        transactions: Vec<HistoryTransaction>,
    ) -> Result<()> {
        self.with_db(move |db| {
            db.transactionally_with_extension::<_, _, anyhow::Error>(|wallet, _| {
                let scanned = wallet
                    .block_fully_scanned()?
                    .context("history requires a scanned checkpoint")?;
                for tx in transactions {
                    anyhow::ensure!(
                        tx.height <= u32::from(scanned.block_height()),
                        "history exceeds the scanned checkpoint"
                    );
                    decrypt_and_store_transaction(
                        &regtest_network(),
                        wallet,
                        &tx.transaction,
                        Some(tx.height.into()),
                    )?;
                }
                Ok(())
            })
        })
        .await
    }

    pub(crate) async fn advance_transparent_history(
        &self,
        query: HistoryQuery,
        checkpoint: ChainCheckpoint,
    ) -> Result<()> {
        let height = u32::try_from(checkpoint.height)?;
        self.with_db(move |db| {
            db.transactionally_with_extension::<_, _, anyhow::Error>(|wallet, ext| {
                let scanned = wallet.block_fully_scanned()?.context("history requires a scanned checkpoint")?;
                if height > u32::from(scanned.block_height())
                    || wallet.get_block_hash(height.into())?.map(|hash|hash.to_string()).as_ref() != Some(&checkpoint.hash) {
                    return Err(crate::bootstrap::ReadinessError::CheckpointChanged.into());
                }
                let current = ext.query_row(
                    "SELECT height,block_hash FROM ext_ths_transparent_history WHERE account_uuid=?1 AND address=?2",
                    rusqlite::params![query.account_id.expose_uuid().to_string(), query.address],
                    |row| Ok(ChainCheckpoint {height: row.get(0)?, hash: row.get(1)?}),
                ).optional()?;
                anyhow::ensure!(current == query.cursor, "transparent history cursor changed during discovery");
                anyhow::ensure!(checkpoint.height > current.as_ref().map_or(u64::from(WALLET_BIRTHDAY_HEIGHT - 1), |cursor| cursor.height), "history cursor must advance");
                ext.execute(
                    "INSERT INTO ext_ths_transparent_history(account_uuid,address,height,block_hash) VALUES(?1,?2,?3,?4)
                     ON CONFLICT(account_uuid,address) DO UPDATE SET height=excluded.height,block_hash=excluded.block_hash",
                    rusqlite::params![query.account_id.expose_uuid().to_string(), query.address, height, checkpoint.hash],
                )?;
                Ok(())
            })
        }).await
    }
}

#[cfg(test)]
#[path = "../../tests/unit/wallet/transparent_history/tests.rs"]
mod tests;
