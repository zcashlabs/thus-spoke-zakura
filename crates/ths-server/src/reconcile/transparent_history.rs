use futures_util::Stream;
use zcash_client_backend::proto::service::{RawTransaction, TransparentAddressBlockFilter};

use super::*;
use crate::wallet::{HistoryQuery, HistoryTransaction};

pub(super) async fn synchronize(
    wallet: &RealWallet,
    rpc: &NodeRpc,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    deadline: Instant,
) -> Result<()> {
    let Some(scanned) = wallet.scanned_checkpoint().await? else {
        return Ok(());
    };
    if within_deadline(
        deadline,
        "validating scanned history checkpoint",
        rpc.block_hash(u32::try_from(scanned.height)?),
    )
    .await?
        != scanned.hash
    {
        return Err(crate::bootstrap::ReadinessError::CheckpointChanged.into());
    }
    loop {
        check_deadline(deadline)?;
        let queries = wallet.transparent_history_queries().await?;
        let mut advanced = false;
        for query in queries {
            let start = query
                .cursor
                .as_ref()
                .map_or(u64::from(WALLET_BIRTHDAY_HEIGHT), |cursor| {
                    cursor.height + 1
                });
            if start > scanned.height {
                continue;
            }
            let end = (start + u64::from(SCAN_BATCH_SIZE) - 1).min(scanned.height);
            let hash = within_deadline(
                deadline,
                "reading transparent history checkpoint",
                rpc.block_hash(u32::try_from(end)?),
            )
            .await?;
            let response = within_deadline(
                deadline,
                "starting transparent transaction history",
                client.get_taddress_transactions(TransparentAddressBlockFilter {
                    address: query.address.clone(),
                    range: Some(BlockRange {
                        start: Some(BlockId {
                            height: start,
                            ..Default::default()
                        }),
                        end: Some(BlockId {
                            height: end,
                            ..Default::default()
                        }),
                        pool_types: vec![],
                    }),
                }),
            )
            .await?;
            ingest_range(
                wallet,
                query,
                ChainCheckpoint { height: end, hash },
                response.into_inner(),
                |height| rpc.block_hash(height),
                |txid, height| verify_transaction(rpc, txid, height),
                deadline,
            )
            .await?;
            advanced = true;
        }
        // Ingestion can extend the receiver lookahead. New receivers must backfill
        // independently before this checkpoint can be published as ready.
        if !advanced {
            return Ok(());
        }
    }
}

async fn verify_transaction(rpc: &NodeRpc, txid: String, height: u32) -> Result<()> {
    let Some(tx) = rpc.lookup_transaction(&txid).await? else {
        return Err(crate::bootstrap::ReadinessError::CheckpointChanged.into());
    };
    if tx["height"].as_u64() != Some(u64::from(height))
        || !tx["confirmations"].as_i64().is_some_and(|n| n > 0)
    {
        return Err(crate::bootstrap::ReadinessError::CheckpointChanged.into());
    }
    let hash = rpc.block_hash(height).await?;
    if tx["blockhash"].as_str() != Some(hash.as_str()) {
        return Err(crate::bootstrap::ReadinessError::CheckpointChanged.into());
    }
    Ok(())
}

async fn ingest_range<S, H, HFut, V, VFut>(
    wallet: &RealWallet,
    query: HistoryQuery,
    checkpoint: ChainCheckpoint,
    stream: S,
    mut canonical_hash: H,
    mut verify: V,
    deadline: Instant,
) -> Result<()>
where
    S: Stream<Item = Result<RawTransaction, tonic::Status>>,
    H: FnMut(u32) -> HFut,
    HFut: Future<Output = Result<String>>,
    V: FnMut(String, u32) -> VFut,
    VFut: Future<Output = Result<()>>,
{
    let start = u32::try_from(
        query
            .cursor
            .as_ref()
            .map_or(u64::from(WALLET_BIRTHDAY_HEIGHT), |cursor| {
                cursor.height + 1
            }),
    )?;
    let end = u32::try_from(checkpoint.height)?;
    anyhow::ensure!(start <= end, "empty transparent history range");
    futures_util::pin_mut!(stream);
    let mut transactions = Vec::with_capacity(SCAN_BATCH_SIZE as usize);
    while let Some(raw) = within_deadline(
        deadline,
        "reading transparent transaction history",
        stream.try_next(),
    )
    .await?
    {
        let tx = within_deadline(
            deadline,
            "decoding transparent transaction history",
            HistoryTransaction::decode(raw, start, end),
        )
        .await?;
        within_deadline(
            deadline,
            "validating transparent history transaction",
            verify(tx.transaction.txid().to_string(), tx.height),
        )
        .await?;
        transactions.push(tx);
        if transactions.len() == SCAN_BATCH_SIZE as usize {
            wallet
                .import_transparent_history(std::mem::take(&mut transactions))
                .await?;
        }
    }
    if !transactions.is_empty() {
        wallet.import_transparent_history(transactions).await?;
    }
    let hash = within_deadline(
        deadline,
        "revalidating transparent history checkpoint",
        canonical_hash(end),
    )
    .await?;
    if hash != checkpoint.hash {
        return Err(crate::bootstrap::ReadinessError::CheckpointChanged.into());
    }
    check_deadline(deadline)?;
    // Completed chunks may survive an interrupted stream. Keeping the cursor
    // behind them makes retry safe; no incomplete range can be skipped.
    wallet
        .advance_transparent_history(query, checkpoint)
        .await?;
    check_deadline(deadline)
}

#[cfg(test)]
#[path = "../../tests/unit/reconcile/transparent_history/tests.rs"]
mod tests;
