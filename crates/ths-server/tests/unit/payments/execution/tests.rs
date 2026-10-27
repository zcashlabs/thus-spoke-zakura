use super::*;
use crate::db::Recipient;
use crate::state::tests::state_with_local_wallet;
use crate::{
    db::{Activity, Store},
    wallet::PreparedPayment,
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
struct RecordingPaymentLifecycle {
    store: Store,
    submissions: Mutex<VecDeque<LifecycleSubmission>>,
    record_fails: AtomicBool,
    confirm: bool,
}

impl RecordingPaymentLifecycle {
    fn new(store: Store, submissions: impl IntoIterator<Item = LifecycleSubmission>) -> Self {
        Self {
            store,
            submissions: Mutex::new(submissions.into_iter().collect()),
            record_fails: AtomicBool::new(false),
            confirm: true,
        }
    }
}

#[async_trait::async_trait]
impl PaymentLifecycle for RecordingPaymentLifecycle {
    async fn submit_prepared(&self, activity: &Activity) -> anyhow::Result<PreparedSubmission> {
        match self
            .submissions
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected submission")
        {
            LifecycleSubmission::Broadcast => Ok(PreparedSubmission::Broadcast(
                self.store.mark_broadcast(&activity.id, &activity.txid)?,
            )),
            LifecycleSubmission::Unavailable => {
                Err(tonic::Status::unavailable("broadcast unavailable").into())
            }
            LifecycleSubmission::LostBroadcast => {
                self.store.mark_broadcast(&activity.id, &activity.txid)?;
                Err(tonic::Status::unavailable("broadcast response lost").into())
            }
            LifecycleSubmission::Expired => Ok(PreparedSubmission::Expired(
                self.store.reset_for_retry(&activity.id, &activity.txid)?,
            )),
        }
    }

    fn record_preparation_error(&self, id: &str, error: &str) -> anyhow::Result<()> {
        self.store.record_error(id, "preparing", "", error)?;
        Ok(())
    }

    fn record_prepared(&self, id: &str, prepared: &PreparedPayment) -> anyhow::Result<Activity> {
        if self.record_fails.load(Ordering::SeqCst) {
            anyhow::bail!("recording failed");
        }
        self.store.record_prepared(
            id,
            &prepared.txid,
            &prepared.raw_transaction,
            prepared.expiry_height,
        )
    }

    async fn confirm_after_mining(&self, activity: Activity) -> anyhow::Result<Activity> {
        if self.confirm {
            self.store
                .confirm(&activity.id, &activity.txid, "confirmed-block")
        } else {
            Ok(activity)
        }
    }
}

fn lifecycle_claim(store: &Store, key: &str) -> Activity {
    store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            key,
            None,
        )
        .unwrap()
}

fn prepared_payment_result(txid: &str, expiry_height: u64) -> PreparedPayment {
    PreparedPayment {
        txid: txid.into(),
        raw_transaction: format!("signed {txid}").into_bytes(),
        expiry_height,
    }
}

#[tokio::test]
async fn payment_lifecycle_submits_prepared_replays_without_preparation() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = lifecycle_claim(&store, "prepared-replay");
    let prepared = store
        .record_prepared(&claim.id, "prepared-txid", b"signed", 140)
        .unwrap();
    let runtime = RecordingPaymentLifecycle::new(store, [LifecycleSubmission::Broadcast]);
    let prepare_calls = Arc::new(AtomicUsize::new(0));

    let result = execute_payment(&runtime, prepared, {
        let prepare_calls = prepare_calls.clone();
        move |_, _| {
            prepare_calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(prepared_payment_result("unused", 180)))
        }
    })
    .await
    .unwrap();

    assert_eq!(result.status, "confirmed");
    assert_eq!(result.txid, "prepared-txid");
    assert_eq!(prepare_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn payment_lifecycle_prepares_again_after_expiry() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = lifecycle_claim(&store, "expired-replay");
    let prepared = store
        .record_prepared(&claim.id, "expired-txid", b"expired", 140)
        .unwrap();
    let runtime = RecordingPaymentLifecycle::new(
        store,
        [LifecycleSubmission::Expired, LifecycleSubmission::Broadcast],
    );
    let prepare_calls = Arc::new(AtomicUsize::new(0));

    let result = execute_payment(&runtime, prepared, {
        let prepare_calls = prepare_calls.clone();
        move |_, replacement_txid| {
            assert_eq!(replacement_txid.as_deref(), Some("expired-txid"));
            prepare_calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(prepared_payment_result("replacement-txid", 180)))
        }
    })
    .await
    .unwrap();

    assert_eq!(result.status, "confirmed");
    assert_eq!(result.txid, "replacement-txid");
    assert_eq!(prepare_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn payment_lifecycle_does_not_discard_after_recording_failure() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = lifecycle_claim(&store, "recording-failure");
    let runtime = RecordingPaymentLifecycle::new(store.clone(), []);
    runtime.record_fails.store(true, Ordering::SeqCst);

    let result = execute_payment(&runtime, claim.clone(), |_, _| {
        std::future::ready(Ok(prepared_payment_result("durable-txid", 180)))
    })
    .await;

    assert!(result.is_err());
    assert_eq!(
        store
            .activity_for_key("recording-failure")
            .unwrap()
            .unwrap()
            .status,
        "preparing"
    );
}

#[tokio::test]
async fn payment_lifecycle_returns_pending_after_confirmation_failure() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = lifecycle_claim(&store, "mining-failure");
    let prepared = store
        .record_prepared(&claim.id, "pending-txid", b"signed", 140)
        .unwrap();
    let mut runtime = RecordingPaymentLifecycle::new(store, [LifecycleSubmission::Broadcast]);
    runtime.confirm = false;

    let result = execute_payment(&runtime, prepared, |_, _| {
        std::future::ready(Ok(prepared_payment_result("unused", 180)))
    })
    .await
    .unwrap();

    assert_eq!(result.status, "broadcast");
    assert_eq!(result.txid, "pending-txid");
}

#[tokio::test]
async fn failed_preparation_preserves_the_claim_and_records_the_error() {
    let (state, _dir) = state_with_local_wallet();
    let claim = state
        .0
        .store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "same",
            None,
        )
        .unwrap();

    assert!(
        execute_payment(&state, claim.clone(), |_, _| {
            std::future::ready(Err(anyhow::anyhow!("preparation failed")))
        })
        .await
        .is_err()
    );
    let retained = state.0.store.activity_for_key("same").unwrap().unwrap();
    assert_eq!(retained.id, claim.id);
    assert_eq!(retained.status, "preparing");
    assert_eq!(
        retained.last_error.as_deref(),
        Some("preparing payment: preparation failed")
    );
}

enum LifecycleSubmission {
    Broadcast,
    Expired,
    Unavailable,
    LostBroadcast,
}

#[tokio::test]
async fn payment_lifecycle_returns_failed_replays_without_preparation() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let claim = lifecycle_claim(&store, "failed-replay");
    let failed = store
        .fail(&claim.id, "insufficient spendable funds")
        .unwrap();
    let runtime = RecordingPaymentLifecycle::new(store, []);
    let result = execute_payment(&runtime, failed.clone(), |_, _| async {
        panic!("failed replay must not prepare another payment")
    })
    .await
    .unwrap();

    assert_eq!(result.id, failed.id);
    assert_eq!(result.status, "failed");
    assert_eq!(
        result.last_error.as_deref(),
        Some("insufficient spendable funds")
    );
}

#[tokio::test(start_paused = true)]
async fn startup_funding_retries_the_original_key_and_signed_bytes_across_failure_windows() {
    for window in ["before-prepare", "after-prepare", "lost-broadcast"] {
        let store = Store::open(":memory:").unwrap();
        store.initialize().unwrap();
        let submissions = match window {
            "before-prepare" => vec![LifecycleSubmission::Broadcast],
            "after-prepare" => vec![
                LifecycleSubmission::Unavailable,
                LifecycleSubmission::Broadcast,
            ],
            _ => vec![
                LifecycleSubmission::LostBroadcast,
                LifecycleSubmission::Broadcast,
            ],
        };
        let runtime = RecordingPaymentLifecycle::new(store.clone(), submissions);
        let prepare_calls = AtomicUsize::new(0);
        crate::bootstrap::retry_startup(|| async {
            let pending =
                store.claim_faucet(1, "ironwood", 500_000_000, "startup-account-1-ironwood-v1")?;
            let activity = execute_payment(&runtime, pending, |_, _| async {
                let attempt = prepare_calls.fetch_add(1, Ordering::SeqCst);
                if window == "before-prepare" && attempt == 0 {
                    return Err(
                        tonic::Status::unavailable("preparation dependency unavailable").into(),
                    );
                }
                Ok(PreparedPayment {
                    txid: "original-startup-txid".into(),
                    raw_transaction: b"original signed payment".to_vec(),
                    expiry_height: 140,
                })
            })
            .await?;
            assert_eq!(activity.status, "confirmed");
            Ok(())
        })
        .await
        .unwrap();
        let activity = store
            .activity_for_key("startup-account-1-ironwood-v1")
            .unwrap()
            .unwrap();
        assert_eq!(activity.status, "confirmed");
        assert_eq!(activity.txid, "original-startup-txid");
        assert_eq!(activity.to_account, Some(1));
        assert_eq!(activity.amount_zatoshi, 500_000_000);
        assert_eq!(store.activities(100).unwrap().len(), 1);
        assert_eq!(
            store.prepared_transaction(&activity.id).unwrap(),
            crate::db::PreparedTransaction {
                raw_transaction: b"original signed payment".to_vec(),
                expiry_height: 140
            }
        );
        assert_eq!(
            prepare_calls.load(Ordering::SeqCst),
            if window == "before-prepare" { 2 } else { 1 }
        );
    }
}
