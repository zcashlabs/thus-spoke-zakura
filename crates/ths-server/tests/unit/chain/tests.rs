use super::*;
use crate::state::tests::{api_request, started_state_with_local_wallet};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::{Notify, Semaphore};

struct Block {
    hash: String,
    txids: Vec<String>,
}

struct FakeChain {
    blocks: Mutex<Vec<Block>>,
    pool: Mutex<Vec<String>>,
    broadcasts: Mutex<Vec<String>>,
    network: &'static str,
    invalidations: AtomicUsize,
    generated: AtomicUsize,
    synced: AtomicUsize,
    fail_generate: usize,
    fail_invalidate: bool,
    fail_sync: bool,
    block_sync: AtomicBool,
    sync_entered: Notify,
    sync_permits: Semaphore,
}

impl FakeChain {
    fn new() -> Self {
        Self {
            blocks: Mutex::new(
                (0..=108)
                    .map(|height| Block {
                        hash: if height == 0 {
                            REGTEST_GENESIS_HASH.into()
                        } else {
                            format!("{height:064x}")
                        },
                        txids: vec![format!("coinbase-{height}")],
                    })
                    .collect(),
            ),
            pool: Mutex::new(Vec::new()),
            broadcasts: Mutex::new(Vec::new()),
            network: "test",
            invalidations: AtomicUsize::new(0),
            generated: AtomicUsize::new(0),
            synced: AtomicUsize::new(0),
            fail_generate: 0,
            fail_invalidate: false,
            fail_sync: false,
            block_sync: AtomicBool::new(false),
            sync_entered: Notify::new(),
            sync_permits: Semaphore::new(0),
        }
    }

    fn add_transaction(&self, height: usize, txid: &str) {
        self.blocks.lock().unwrap()[height].txids.push(txid.into());
    }
}

#[async_trait::async_trait]
impl ChainRuntime for FakeChain {
    async fn chain_info(&self) -> Result<ChainInfo> {
        let blocks = self.blocks.lock().unwrap();
        Ok(ChainInfo {
            chain: self.network.into(),
            blocks: (blocks.len() - 1) as u64,
            bestblockhash: blocks.last().unwrap().hash.clone(),
            verificationprogress: 1.0,
        })
    }
    async fn block_hash(&self, height: u32) -> Result<String> {
        Ok(self.blocks.lock().unwrap()[height as usize].hash.clone())
    }
    async fn block(&self, hash: &str) -> Result<Value> {
        let blocks = self.blocks.lock().unwrap();
        let (height, block) = blocks
            .iter()
            .enumerate()
            .find(|(_, block)| block.hash == hash)
            .unwrap();
        Ok(
            serde_json::json!({"height":height,"hash":hash,"tx":block.txids.iter().map(|id|
            serde_json::json!({"txid":id})).collect::<Vec<_>>()}),
        )
    }
    async fn raw_transaction(&self, txid: &str) -> Result<Vec<u8>> {
        Ok(txid.as_bytes().to_vec())
    }
    async fn invalidate(&self, hash: &str) -> Result<()> {
        self.invalidations.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!self.fail_invalidate, "injected invalidation rejection");
        let mut blocks = self.blocks.lock().unwrap();
        let index = blocks.iter().position(|block| block.hash == hash).unwrap();
        blocks.truncate(index);
        Ok(())
    }
    async fn broadcast(&self, raw: &[u8]) -> Result<()> {
        let txid = std::str::from_utf8(raw)?.to_owned();
        self.broadcasts.lock().unwrap().push(txid.clone());
        if txid == "queued" {
            return Ok(());
        }
        anyhow::ensure!(txid != "invalid-anchor", "anchor is no longer canonical");
        if txid == "dependent" {
            anyhow::ensure!(
                self.blocks.lock().unwrap().len() > 106,
                "parent transaction must be mined first"
            );
        }
        let mut pool = self.pool.lock().unwrap();
        if !pool.contains(&txid) {
            pool.push(txid);
        }
        Ok(())
    }
    async fn mempool(&self) -> Result<Vec<String>> {
        Ok(self.pool.lock().unwrap().clone())
    }
    async fn generate_one(&self) -> Result<String> {
        let call = self.generated.fetch_add(1, Ordering::SeqCst) + 1;
        anyhow::ensure!(call != self.fail_generate, "injected generation failure");
        let mut blocks = self.blocks.lock().unwrap();
        let height = blocks.len();
        let hash = format!("{:064x}", 1000 + height + call * 10_000);
        let mut txids = vec![format!("new-coinbase-{height}")];
        txids.append(&mut self.pool.lock().unwrap());
        blocks.push(Block {
            hash: hash.clone(),
            txids,
        });
        Ok(hash)
    }
    async fn synchronize(&self) -> Result<()> {
        self.synced.fetch_add(1, Ordering::SeqCst);
        self.sync_entered.notify_one();
        if self.block_sync.load(Ordering::SeqCst) {
            self.sync_permits.acquire().await?.forget();
        }
        anyhow::ensure!(!self.fail_sync, "injected synchronization failure");
        Ok(())
    }
}

fn store() -> Store {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    store
}

fn confirmed_payment(store: &Store, hash: &str) -> crate::db::Activity {
    let payment = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "reorg-original-key",
            Some("memo"),
        )
        .unwrap();
    let payment = store
        .record_prepared(&payment.id, "payment", b"signed bytes", 140)
        .unwrap();
    store.confirm(&payment.id, &payment.txid, hash).unwrap()
}

#[test]
fn depth_retains_the_requested_ancestor_and_respects_both_floors() {
    for height in [0, 1, 2, 108, 1000, 1001, 2000, u64::from(u32::MAX)] {
        for depth in 0..=1001 {
            match validate_depth(depth, height) {
                Ok(ancestor) => {
                    assert!((1..=1000).contains(&depth));
                    assert!(ancestor >= 1);
                    assert_eq!(u64::from(ancestor) + u64::from(depth), height);
                }
                Err(_) => assert!(depth == 0 || depth > 1000 || u64::from(depth) >= height),
            }
        }
    }
    assert_eq!(validate_depth(1000, 1001).unwrap(), 1);
    assert!(validate_depth(1, u64::MAX).is_err());
}

#[tokio::test]
async fn reorg_replaces_exactly_the_suffix_and_preserves_the_payment_identity() {
    let runtime = FakeChain::new();
    let store = store();
    let original = confirmed_payment(&store, &runtime.block_hash(106).await.unwrap());
    let result = change_chain_with(&runtime, &store, 3, true, false)
        .await
        .unwrap();
    assert_eq!(result.old_tip.height, 108);
    assert_eq!(result.new_tip.height, 108);
    assert_eq!(
        result.removed_hashes,
        [106, 107, 108].map(|height| format!("{height:064x}"))
    );
    assert_eq!(result.replacement_hashes.len(), 3);
    assert_eq!(
        runtime.block_hash(105).await.unwrap(),
        format!("{:064x}", 105)
    );
    assert!(
        result
            .removed_hashes
            .iter()
            .zip(&result.replacement_hashes)
            .all(|(old, new)| old != new)
    );
    assert!(runtime.broadcasts.lock().unwrap().is_empty());
    assert_eq!(runtime.synced.load(Ordering::SeqCst), 1);
    let orphan = store
        .activity_for_key("reorg-original-key")
        .unwrap()
        .unwrap();
    assert_eq!(orphan.id, original.id);
    assert_eq!(orphan.txid, original.txid);
    assert_eq!(orphan.status, "unconfirmed");
    assert_eq!(orphan.block_hash, None);
    assert_eq!(
        store
            .prepared_transaction(&orphan.id)
            .unwrap()
            .raw_transaction,
        b"signed bytes"
    );
    assert!(
        store
            .claim_transfer(
                1,
                crate::db::Recipient::Account(2),
                "ironwood",
                "ironwood",
                13_000,
                "reorg-original-key",
                Some("memo")
            )
            .is_err()
    );
    let next = change_chain_with(&runtime, &store, 3, true, false)
        .await
        .unwrap();
    assert_eq!(next.new_tip.height, 108);
    assert_eq!(next.removed_hashes, result.replacement_hashes);
    assert!(
        next.removed_hashes
            .iter()
            .zip(&next.replacement_hashes)
            .all(|(old, new)| old != new)
    );
    assert_eq!(store.activity(&original.id).unwrap().status, "unconfirmed");
}

#[tokio::test]
async fn rollback_does_not_mine_and_invalid_depth_does_not_mutate() {
    for depth in [0, 108, 1001] {
        let runtime = FakeChain::new();
        assert!(
            change_chain_with(&runtime, &store(), depth, false, false)
                .await
                .is_err()
        );
        assert_eq!(runtime.invalidations.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.generated.load(Ordering::SeqCst), 0);
    }
    let runtime = FakeChain::new();
    let result = change_chain_with(&runtime, &store(), 3, false, false)
        .await
        .unwrap();
    assert_eq!(result.new_tip.height, 105);
    assert_eq!(result.new_tip.hash, format!("{:064x}", 105));
    assert!(result.replacement_hashes.is_empty());
    assert_eq!(runtime.generated.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.synced.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn regtest_is_identified_despite_the_shared_testnet_rpc_label() {
    let runtime = FakeChain::new();
    let result = change_chain_with(&runtime, &store(), 3, true, false)
        .await
        .unwrap();
    assert_eq!(result.new_tip.height, 108);
    assert_eq!(result.replacement_hashes.len(), 3);
}

#[tokio::test]
async fn chain_changes_reject_other_networks_before_any_mutation() {
    for network in ["main", "test"] {
        let mut runtime = FakeChain::new();
        runtime.network = network;
        runtime.blocks.lock().unwrap()[0].hash =
            "05a60a92d99d85997cce3b87616c089f6124d7342af37106edc76126334a2c38".into();
        assert!(
            change_chain_with(&runtime, &store(), 3, true, true)
                .await
                .is_err()
        );
        assert_eq!(runtime.invalidations.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.generated.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn accepted_broadcast_without_inclusion_is_reported_as_a_retention_failure() {
    let runtime = FakeChain::new();
    runtime.add_transaction(108, "queued");
    let result = change_chain_with(&runtime, &store(), 1, true, true)
        .await
        .unwrap();
    assert_eq!(result.new_tip.height, 108);
    assert!(result.retained_txids.is_empty());
    assert_eq!(result.failed_transactions.len(), 1);
    assert_eq!(result.failed_transactions[0].txid, "queued");
    assert!(
        result.failed_transactions[0]
            .error
            .contains("admission timed out")
    );
}

#[tokio::test]
async fn keep_txs_retries_dependencies_and_reports_invalid_transactions() {
    let runtime = FakeChain::new();
    runtime.add_transaction(106, "payment");
    runtime.add_transaction(107, "dependent");
    runtime.add_transaction(108, "invalid-anchor");
    let store = store();
    let original = confirmed_payment(&store, &runtime.block_hash(106).await.unwrap());
    let result = change_chain_with(&runtime, &store, 3, true, true)
        .await
        .unwrap();
    assert_eq!(result.retained_txids, ["payment", "dependent"]);
    assert_eq!(result.failed_transactions.len(), 1);
    assert_eq!(result.failed_transactions[0].txid, "invalid-anchor");
    assert!(result.failed_transactions[0].error.contains("anchor"));
    let current = store.activity(&original.id).unwrap();
    assert_eq!(current.txid, original.txid);
    assert_eq!(current.status, "confirmed");
    assert_eq!(
        current.block_hash.as_ref(),
        result.replacement_hashes.first()
    );
    assert!(
        runtime
            .broadcasts
            .lock()
            .unwrap()
            .iter()
            .all(|id| !id.starts_with("coinbase"))
    );
}

#[tokio::test]
async fn failed_mutations_reconcile_without_retrying_chain_changes() {
    for (invalidate, generate, sync) in [(true, 0, false), (false, 2, false), (false, 0, true)] {
        let mut runtime = FakeChain::new();
        runtime.fail_invalidate = invalidate;
        runtime.fail_generate = generate;
        runtime.fail_sync = sync;
        let store = store();
        let original = confirmed_payment(&store, &runtime.block_hash(106).await.unwrap());
        assert!(
            change_chain_with(&runtime, &store, 3, true, false)
                .await
                .is_err()
        );
        assert_eq!(runtime.invalidations.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.synced.load(Ordering::SeqCst), 1);
        let payment = store.activity(&original.id).unwrap();
        assert_eq!(
            payment.status,
            if invalidate {
                "confirmed"
            } else {
                "unconfirmed"
            }
        );
        assert_eq!(
            payment.block_hash,
            if invalidate {
                original.block_hash
            } else {
                None
            }
        );
        assert_eq!(
            runtime.chain_info().await.unwrap().blocks,
            if invalidate {
                108
            } else if generate == 2 {
                106
            } else {
                108
            }
        );
    }
}

#[tokio::test]
async fn admission_rejects_other_chain_work_and_http_disconnect_does_not_cancel() {
    let (mut state, _dir) = started_state_with_local_wallet();
    let runtime = Arc::new(FakeChain::new());
    Arc::get_mut(&mut state.0).unwrap().chain_runtime = Some(runtime.clone());
    for depth in [0, 1001] {
        let (status, _) = api_request(
            &state,
            "POST",
            "/api/v1/reorg",
            serde_json::json!({"depth":depth}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    }
    let payment = state.0.payments.lock().await;
    assert!(state.change_chain(3, true, false).await.is_err());
    drop(payment);
    let manual = state.0.mining.chain_operations.read().await;
    assert!(matches!(
        state
            .change_chain(3, true, false)
            .await
            .unwrap_err()
            .downcast_ref::<ChainChangeError>(),
        Some(ChainChangeError::Busy)
    ));
    drop(manual);
    assert_eq!(runtime.invalidations.load(Ordering::SeqCst), 0);
    runtime.block_sync.store(true, Ordering::SeqCst);
    let entered = runtime.sync_entered.notified();
    let request_state = state.clone();
    let request = tokio::spawn(async move {
        api_request(
            &request_state,
            "POST",
            "/api/v1/reorg",
            serde_json::json!({"depth":3}),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), entered)
        .await
        .unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert!(state.0.mining.chain_operations.try_read().is_err());
    let (status, _) = api_request(
        &state,
        "POST",
        "/api/v1/rollback",
        serde_json::json!({"depth":1}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    runtime.sync_permits.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.0.mining.chain_operations.try_write().is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(runtime.generated.load(Ordering::SeqCst), 3);
    assert_eq!(runtime.chain_info().await.unwrap().blocks, 108);
}
