use super::*;
use crate::db::Store;
use zcash_primitives::block::BlockHash;

#[tokio::test]
async fn same_height_and_lower_tip_reorgs_find_the_last_common_block() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    assert_eq!(
        find_rewind(
            &wallet,
            &ChainCheckpoint {
                height: 0,
                hash: "00".repeat(32)
            },
            Instant::now() + Duration::from_secs(1),
            |_| async { Ok("00".repeat(32)) },
        )
        .await
        .unwrap(),
        None
    );
    wallet.update_chain_tip(4).await.unwrap();
    let blocks = (2..=4)
        .map(|height| CompactBlock {
            height,
            hash: vec![height as u8; 32],
            prev_hash: vec![(height - 1) as u8; 32],
            chain_metadata: Some(Default::default()),
            ..Default::default()
        })
        .collect();
    wallet
        .scan_batch(
            ScanRange::from_parts(2.into()..5.into(), ScanPriority::Historic),
            blocks,
            ChainState::empty(1.into(), BlockHash([1; 32])),
        )
        .await
        .unwrap();
    assert_eq!(
        wallet.scanned_checkpoint().await.unwrap().unwrap().height,
        4
    );

    for tip in [4, 3] {
        let rewind = find_rewind(
            &wallet,
            &ChainCheckpoint {
                height: tip,
                hash: "ff".repeat(32),
            },
            Instant::now() + Duration::from_secs(1),
            |height| async move {
                Ok(if height <= 2 {
                    BlockHash([height as u8; 32]).to_string()
                } else {
                    "ff".repeat(32)
                })
            },
        )
        .await
        .unwrap();
        assert_eq!(
            rewind,
            Some(ChainCheckpoint {
                height: 2,
                hash: "02".repeat(32)
            })
        );
    }

    assert!(
        find_rewind(
            &wallet,
            &ChainCheckpoint {
                height: 0,
                hash: "00".repeat(32)
            },
            Instant::now() + Duration::from_secs(1),
            |_| async { Ok("00".repeat(32)) },
        )
        .await
        .is_err()
    );
    assert_eq!(
        wallet.scanned_checkpoint().await.unwrap().unwrap().height,
        4
    );
    assert_eq!(
        find_rewind(
            &wallet,
            &ChainCheckpoint {
                height: 1,
                hash: "01".repeat(32)
            },
            Instant::now() + Duration::from_secs(1),
            |_| async { Ok("01".repeat(32)) },
        )
        .await
        .unwrap(),
        Some(ChainCheckpoint {
            height: 1,
            hash: "01".repeat(32)
        })
    );

    wallet
        .rewind_to_height(ChainState::empty(2.into(), BlockHash([2; 32])))
        .await
        .unwrap();
    assert_eq!(
        wallet.scanned_checkpoint().await.unwrap().unwrap().height,
        2
    );
    wallet.update_chain_tip(3).await.unwrap();
    wallet
        .scan_batch(
            ScanRange::from_parts(3.into()..4.into(), ScanPriority::Historic),
            vec![CompactBlock {
                height: 3,
                hash: vec![255; 32],
                prev_hash: vec![2; 32],
                chain_metadata: Some(Default::default()),
                ..Default::default()
            }],
            ChainState::empty(2.into(), BlockHash([2; 32])),
        )
        .await
        .unwrap();
    assert_eq!(
        wallet.scanned_checkpoint().await.unwrap(),
        Some(ChainCheckpoint {
            height: 3,
            hash: "ff".repeat(32),
        })
    );

    wallet
        .rewind_to_height(ChainState::empty(1.into(), BlockHash([1; 32])))
        .await
        .unwrap();
    assert_eq!(wallet.scanned_checkpoint().await.unwrap(), None);
}

#[tokio::test]
async fn one_attempt_deadline_is_shared_across_network_calls() {
    let deadline = Instant::now() + Duration::from_millis(100);
    within_deadline(deadline, "first call", async {
        Ok::<_, std::io::Error>(())
    })
    .await
    .unwrap();
    let operation = async {
        tokio::time::sleep_until(deadline + Duration::from_millis(50)).await;
        Ok::<_, std::io::Error>(())
    };
    let error = within_deadline(deadline, "second call", operation)
        .await
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
    );
    let polled = std::cell::Cell::new(false);
    let error = within_deadline(deadline, "expired call", async {
        polled.set(true);
        Ok::<_, std::io::Error>(())
    })
    .await
    .unwrap_err();
    assert!(error.downcast_ref::<ReconciliationDeadline>().is_some());
    assert!(
        !polled.get(),
        "expired attempts must not start another call"
    );
}

#[test]
fn scan_ranges_are_split_without_gaps() {
    let range = ScanRange::from_parts(
        BlockHeight::from_u32(5)..BlockHeight::from_u32(256),
        ScanPriority::Historic,
    );
    let ranges = split_range(range)
        .map(|range| {
            (
                u32::from(range.block_range().start),
                u32::from(range.block_range().end),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(ranges, [(5, 105), (105, 205), (205, 256)]);
}

#[test]
fn only_an_aborted_lightwalletd_stream_marks_a_reorg_repair() {
    let aborted = within_deadline_error(tonic::Status::aborted(
        "GetBlockRange: chain discontinuity during reorg repair",
    ));
    assert!(repairing_reorg(&aborted));
    let out_of_range = within_deadline_error(tonic::Status::out_of_range(
        "GetBlock: block 5 is newer than the latest block",
    ));
    assert!(!repairing_reorg(&out_of_range));
    assert!(!repairing_reorg(&anyhow::anyhow!(
        "streaming compact blocks"
    )));
}

/// The error `within_deadline` returns for a lightwalletd status: the status wrapped in the
/// call's label, as `sync_pass` sees it.
fn within_deadline_error(status: tonic::Status) -> anyhow::Error {
    anyhow::Error::from(status).context("streaming compact blocks")
}
