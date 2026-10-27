use std::{future::Future, time::Duration};

use anyhow::{Context, Result};
use futures_util::TryStreamExt;
use tokio::time::{Instant, timeout_at};
use zcash_client_backend::{
    data_api::{
        chain::ChainState,
        scanning::{ScanPriority, ScanRange},
    },
    proto::{
        compact_formats::CompactBlock,
        service::{
            BlockId, BlockRange, ChainSpec, GetAddressUtxosArg, GetSubtreeRootsArg,
            ShieldedProtocol as ProtoShieldedProtocol, TxFilter,
            compact_tx_streamer_client::CompactTxStreamerClient,
        },
    },
};
use zcash_protocol::consensus::BlockHeight;

use crate::rpc::{ChainCheckpoint, NodeRpc};
use crate::wallet::{
    RealWallet, ScanOutcome, ShieldedProtocol, SubtreeRootData, TransparentOutputData,
    TransparentQuery, WALLET_BIRTHDAY_HEIGHT,
};

#[derive(Debug, thiserror::Error)]
#[error("wallet reconciliation deadline exceeded")]
pub(crate) struct ReconciliationDeadline;

const NETWORK_TIMEOUT: Duration = Duration::from_secs(30);
const SCAN_BATCH_SIZE: u32 = 100;
const REORG_REPAIR_POLL: Duration = Duration::from_millis(250);
const ENHANCEMENT_BATCH_SIZE: usize = 100;

mod transparent_history;

pub(crate) async fn find_rewind<F, Fut>(
    wallet: &RealWallet,
    target: &ChainCheckpoint,
    deadline: Instant,
    mut canonical_hash: F,
) -> Result<Option<ChainCheckpoint>>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let Some(scanned) = wallet.max_scanned_checkpoint().await? else {
        return Ok(None);
    };
    if target.height < u64::from(WALLET_BIRTHDAY_HEIGHT - 1) {
        return Err(crate::bootstrap::ReadinessError::BelowBirthday.into());
    }
    let mut height = u32::try_from(scanned.height.min(target.height))?;
    loop {
        check_deadline(deadline)?;
        let hash =
            within_deadline(deadline, "validating wallet block", canonical_hash(height)).await?;
        if wallet.block_hash(height).await?.as_ref() == Some(&hash) {
            return Ok(
                (u64::from(height) != scanned.height).then_some(ChainCheckpoint {
                    height: u64::from(height),
                    hash,
                }),
            );
        }
        if height < WALLET_BIRTHDAY_HEIGHT {
            return Ok(Some(ChainCheckpoint {
                height: u64::from(height),
                hash,
            }));
        }
        height -= 1;
    }
}

pub async fn sync_wallet(
    wallet: &RealWallet,
    rpc: &NodeRpc,
    target: &ChainCheckpoint,
    deadline: Instant,
) -> Result<()> {
    let mut rewind = find_rewind(wallet, target, deadline, |height| rpc.block_hash(height)).await?;
    for cursor in wallet.transparent_history_checkpoints().await? {
        let canonical = if cursor.height <= target.height {
            Some(
                within_deadline(
                    deadline,
                    "validating transparent history cursor",
                    rpc.block_hash(u32::try_from(cursor.height)?),
                )
                .await?,
            )
        } else {
            None
        };
        if canonical.as_deref() != Some(cursor.hash.as_str()) {
            let floor = WALLET_BIRTHDAY_HEIGHT - 1;
            rewind = Some(ChainCheckpoint {
                height: u64::from(floor),
                hash: within_deadline(
                    deadline,
                    "validating transparent history rewind floor",
                    rpc.block_hash(floor),
                )
                .await?,
            });
            break;
        }
    }
    if let Some(cursor) = wallet.treasury_cursor().await? {
        let canonical = if target.height >= u64::from(cursor.height) {
            Some(
                within_deadline(
                    deadline,
                    "validating treasury discovery cursor",
                    rpc.block_hash(cursor.height),
                )
                .await?,
            )
        } else {
            None
        };
        if canonical.as_deref() != Some(cursor.block_hash.as_str()) {
            let floor = WALLET_BIRTHDAY_HEIGHT - 1;
            if target.height < u64::from(floor) {
                return Err(crate::bootstrap::ReadinessError::BelowBirthday.into());
            }
            rewind = Some(ChainCheckpoint {
                height: u64::from(floor),
                hash: within_deadline(
                    deadline,
                    "validating treasury rewind floor",
                    rpc.block_hash(floor),
                )
                .await?,
            });
        }
    }
    let mut client = within_deadline(
        deadline,
        "connecting to lightwalletd",
        CompactTxStreamerClient::connect(wallet.lightwalletd().to_owned()),
    )
    .await?;
    if let Some(checkpoint) = rewind {
        rewind_wallet(wallet, &mut client, &checkpoint, deadline).await?;
    }
    update_subtree_roots(wallet, &mut client, deadline).await?;
    loop {
        match sync_pass(wallet, rpc, &mut client, deadline).await {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) if repairing_reorg(&error) => {
                check_deadline(deadline)?;
                tokio::time::sleep(REORG_REPAIR_POLL).await;
            }
            Err(error) => return Err(error),
        }
    }
    enhance_transactions(wallet, &mut client, deadline).await?;
    transparent_history::synchronize(wallet, rpc, &mut client, deadline).await?;
    check_deadline(deadline)
}

/// lightwalletd aborts `GetBlockRange` while its ingestor replaces the blocks a rollback removed:
/// until it has re-indexed them its cache still ends in the old chain, and it refuses to stream
/// those blocks together with the new ones it reads straight from Zakura. The blocks become
/// readable once the repair finishes, so the scan retries instead of failing the sync.
fn repairing_reorg(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<tonic::Status>()
            .is_some_and(|status| status.code() == tonic::Code::Aborted)
    })
}

async fn enhance_transactions(
    wallet: &RealWallet,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    deadline: Instant,
) -> Result<()> {
    for txid in wallet
        .public_transaction_enhancements(ENHANCEMENT_BATCH_SIZE)
        .await?
    {
        let response = within_deadline(
            deadline,
            "downloading transaction enhancement",
            client.get_transaction(TxFilter {
                block: None,
                index: 0,
                hash: txid.as_ref().to_vec(),
            }),
        )
        .await;
        let raw = match response {
            Ok(response) => response.into_inner(),
            Err(error)
                if error
                    .downcast_ref::<tonic::Status>()
                    .is_some_and(|status| status.code() == tonic::Code::NotFound) =>
            {
                wallet.transaction_enhancement_not_found(txid).await?;
                continue;
            }
            Err(error) => return Err(error),
        };
        let height = u32::try_from(raw.height)
            .context("lightwalletd returned an invalid transaction enhancement height")?;
        anyhow::ensure!(
            height > 0,
            "lightwalletd returned an unmined transaction enhancement"
        );
        check_deadline(deadline)?;
        wallet.enhance_transaction(txid, raw.data, height).await?;
    }
    Ok(())
}

async fn rewind_wallet(
    wallet: &RealWallet,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    checkpoint: &ChainCheckpoint,
    deadline: Instant,
) -> Result<()> {
    let height = u32::try_from(checkpoint.height)?;
    let chain_state = download_chain_state(client, height.into(), deadline).await?;
    if chain_state.block_height() != height.into()
        || chain_state.block_hash().to_string() != checkpoint.hash
    {
        return Err(crate::bootstrap::ReadinessError::RewindCheckpoint.into());
    }
    check_deadline(deadline)?;
    wallet.rewind_to_height(chain_state).await?;
    check_deadline(deadline)
}

async fn sync_pass(
    wallet: &RealWallet,
    rpc: &NodeRpc,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    deadline: Instant,
) -> Result<bool> {
    check_deadline(deadline)?;
    let tip = within_deadline(
        deadline,
        "reading the lightwalletd tip",
        client.get_latest_block(ChainSpec::default()),
    )
    .await?
    .into_inner()
    .height;
    wallet.update_chain_tip(tip).await?;

    for query in wallet.public_transparent_queries().await? {
        refresh_transparent_outputs(wallet, client, query, deadline).await?;
    }

    let mut ranges = wallet.suggested_scan_ranges().await?;
    loop {
        match ranges.first() {
            Some(range) if range.priority() == ScanPriority::Verify => {
                if scan_range(wallet, rpc, client, range.clone(), deadline).await? {
                    ranges = wallet.suggested_scan_ranges().await?;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }

    for range in wallet.suggested_scan_ranges().await? {
        for batch in split_range(range) {
            if scan_range(wallet, rpc, client, batch, deadline).await? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

async fn update_subtree_roots(
    wallet: &RealWallet,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    deadline: Instant,
) -> Result<()> {
    for (wire, wallet_protocol) in [
        (ProtoShieldedProtocol::Sapling, ShieldedProtocol::Sapling),
        (ProtoShieldedProtocol::Orchard, ShieldedProtocol::Orchard),
        (ProtoShieldedProtocol::Ironwood, ShieldedProtocol::Ironwood),
    ] {
        let mut request = GetSubtreeRootsArg::default();
        request.set_shielded_protocol(wire);
        let response = within_deadline(
            deadline,
            "downloading shielded subtree roots",
            client.get_subtree_roots(request),
        )
        .await?;
        let roots = within_deadline(
            deadline,
            "streaming shielded subtree roots",
            response
                .into_inner()
                .map_ok(|root| SubtreeRootData {
                    completing_height: root.completing_block_height as u32,
                    root_hash: root.root_hash,
                })
                .try_collect::<Vec<_>>(),
        )
        .await?;
        wallet.put_subtree_roots(wallet_protocol, roots).await?;
    }
    Ok(())
}

async fn refresh_transparent_outputs(
    wallet: &RealWallet,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    query: TransparentQuery,
    deadline: Instant,
) -> Result<()> {
    if query.addresses.is_empty() {
        return Ok(());
    }
    let response = within_deadline(
        deadline,
        "starting transparent output discovery",
        client.get_address_utxos_stream(GetAddressUtxosArg {
            addresses: query.addresses,
            start_height: query.start_height,
            max_entries: 0,
        }),
    )
    .await?;
    let mut stream = response.into_inner();
    let mut outputs = Vec::with_capacity(SCAN_BATCH_SIZE as usize);
    loop {
        let next =
            within_deadline(deadline, "reading transparent outputs", stream.try_next()).await?;
        let Some(reply) = next else {
            break;
        };
        outputs.push(TransparentOutputData {
            txid: reply
                .txid
                .try_into()
                .map_err(|_| anyhow::anyhow!("lightwalletd returned an invalid UTXO txid"))?,
            index: reply
                .index
                .try_into()
                .map_err(|_| anyhow::anyhow!("lightwalletd returned an invalid UTXO index"))?,
            value_zatoshi: reply.value_zat,
            script: reply.script,
            height: reply
                .height
                .try_into()
                .map_err(|_| anyhow::anyhow!("lightwalletd returned an invalid UTXO height"))?,
            account_id: query.account_id,
        });
        if outputs.len() == SCAN_BATCH_SIZE as usize {
            wallet
                .insert_transparent_outputs(std::mem::take(&mut outputs))
                .await?;
        }
    }
    if !outputs.is_empty() {
        wallet.insert_transparent_outputs(outputs).await?;
    }
    Ok(())
}

async fn scan_range(
    wallet: &RealWallet,
    rpc: &NodeRpc,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    range: ScanRange,
    deadline: Instant,
) -> Result<bool> {
    check_deadline(deadline)?;
    let block_range = range.block_range();
    if block_range.is_empty() {
        return Ok(false);
    }
    let blocks = download_blocks(client, &range, deadline).await?;
    let chain_state = download_chain_state(client, block_range.start - 1, deadline).await?;
    check_deadline(deadline)?;
    let outcome = wallet.scan_batch(range, blocks, chain_state).await?;
    check_deadline(deadline)?;
    match outcome {
        ScanOutcome::Scanned(ranges_changed) => Ok(ranges_changed),
        ScanOutcome::Rewind(height) => {
            let hash = within_deadline(
                deadline,
                "validating continuity rewind",
                rpc.block_hash(height),
            )
            .await?;
            rewind_wallet(
                wallet,
                client,
                &ChainCheckpoint {
                    height: u64::from(height),
                    hash,
                },
                deadline,
            )
            .await?;
            Ok(true)
        }
    }
}

async fn download_blocks(
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    range: &ScanRange,
    deadline: Instant,
) -> Result<Vec<CompactBlock>> {
    let mut start = BlockId::default();
    start.height = range.block_range().start.into();
    let mut end = BlockId::default();
    end.height = (range.block_range().end - 1).into();
    let response = within_deadline(
        deadline,
        "downloading compact blocks",
        client.get_block_range(BlockRange {
            start: Some(start),
            end: Some(end),
            pool_types: vec![],
        }),
    )
    .await?;
    within_deadline(
        deadline,
        "streaming compact blocks",
        response.into_inner().try_collect::<Vec<_>>(),
    )
    .await
}

async fn download_chain_state(
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
    height: BlockHeight,
    deadline: Instant,
) -> Result<ChainState> {
    within_deadline(
        deadline,
        "downloading compact-block tree state",
        client.get_tree_state(BlockId {
            height: height.into(),
            hash: vec![],
        }),
    )
    .await?
    .into_inner()
    .to_chain_state()
    .context("lightwalletd returned an invalid tree state")
}

fn split_range(range: ScanRange) -> impl Iterator<Item = ScanRange> {
    (0..).scan(range, |remaining, _| {
        if remaining.is_empty() {
            None
        } else if let Some((current, next)) =
            remaining.split_at(remaining.block_range().start + SCAN_BATCH_SIZE)
        {
            *remaining = next;
            Some(current)
        } else {
            let current = remaining.clone();
            let end = remaining.block_range().end;
            *remaining = ScanRange::from_parts(end..end, remaining.priority());
            Some(current)
        }
    })
}

pub(crate) fn check_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        return Err(ReconciliationDeadline.into());
    }
    Ok(())
}

pub(crate) async fn within_deadline<T, E>(
    deadline: Instant,
    label: &str,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T>
where
    E: Into<anyhow::Error>,
{
    check_deadline(deadline)?;
    match timeout_at(deadline.min(Instant::now() + NETWORK_TIMEOUT), future).await {
        Ok(result) => result.map_err(Into::into).with_context(|| label.to_owned()),
        Err(error) => Err(error)
            .with_context(|| format!("{label} exceeded the network or reconciliation deadline")),
    }
}

#[cfg(test)]
#[path = "../tests/unit/reconcile/tests.rs"]
mod tests;
