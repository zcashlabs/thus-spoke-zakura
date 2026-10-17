mod support;

#[path = "scenarios/activity_recovery/chain.rs"]
mod chain;

#[path = "scenarios/activity_recovery/assertions.rs"]
mod assertions;
#[path = "scenarios/activity_recovery/faucet.rs"]
mod faucet;
#[path = "scenarios/activity_recovery/recovery.rs"]
mod recovery;
#[path = "scenarios/activity_recovery/send.rs"]
mod send;
#[path = "scenarios/activity_recovery/treasury.rs"]
mod treasury;

use assertions::{
    assert_address_faucet_inclusion, assert_persisted_send_response, assert_read_only_persistence,
    assert_requested_payment_fields, constructed_transactions, one_matching_activity,
    pending_payment_snapshot, prepared_payment, prepared_payment_journal,
    preserve_scenario_failure,
};
use recovery::{exercise_recovery, queued_retry_survives_restart};
use treasury::exercise_treasury_sync;

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use reqwest::Client;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use transparent::address::TransparentAddress;
use zcash_keys::address::Address;
use zcash_protocol::{consensus::BlockHeight, local_consensus::LocalNetwork};

use support::{
    FailureRoute, GenerateCounts, HeightCheckpoint, QueuedBroadcastProxy, RecoveryFailureReporter,
    RecoveryPhase, RegtestStack, TerminationSignals, lose_payment_response, request_json, rpc,
    rpc_with_timeout,
};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn reorg_and_rollback_preserve_canonical_wallet_and_payment_state() -> Result<()> {
    chain::reorg_and_rollback_preserve_canonical_wallet_and_payment_state().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn queued_send_retry_survives_lost_response_and_restart() -> Result<()> {
    send::queued_send_retry_survives_lost_response_and_restart().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn queued_external_faucet_retry_survives_lost_response_and_restart() -> Result<()> {
    faucet::queued_external_faucet_retry_survives_lost_response_and_restart().await
}

#[derive(PartialEq, Eq)]
struct PendingPaymentSnapshot {
    id: String,
    txid: String,
    raw: Vec<u8>,
    expiry_height: u64,
    reserved_inputs: Vec<(String, i64)>,
}

const RECOVERY_IDEMPOTENCY_KEY: &str = "recovery-after-auto-mine-failure";
const CONCURRENT_IDEMPOTENCY_KEY: &str = "concurrent-identical-send";
const PREPARE_ROLLBACK_IDEMPOTENCY_KEY: &str = "prepare-journal-rollback";
const PREPARE_RESTART_IDEMPOTENCY_KEY: &str = "prepare-restart-recovery";
const RECOVERY_POLL_INTERVAL: Duration = Duration::from_millis(250);
const API_READ_TIMEOUT: Duration = Duration::from_secs(5);
const SEND_TIMEOUT: Duration = Duration::from_secs(120);
const LARGE_MINE_TIMEOUT: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy)]
enum LiveScenario {
    BroadcastRecovery,
    TreasurySync,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct Activity {
    id: String,
    kind: String,
    from_account: Option<u8>,
    to_account: Option<u8>,
    to_address: Option<String>,
    source_pool: String,
    destination_pool: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    status: String,
    created_at: String,
    last_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChainInfo {
    blocks: u64,
    bestblockhash: String,
}

#[derive(Debug, Deserialize)]
struct TransactionEvidence {
    confirmations: Option<u64>,
    blockhash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Status {
    wallet_sync: SyncStatus,
}

#[derive(Debug, Deserialize)]
struct SyncStatus {
    state: String,
    fully_scanned_height: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AccountBalance {
    id: u8,
    ironwood_zatoshi: u64,
    transparent_zatoshi: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct ConstructedTransactionRecord {
    txid: Vec<u8>,
    raw_transaction: Vec<u8>,
    expiry_height: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct PreparedPaymentRecord {
    txid: String,
    raw_transaction: Vec<u8>,
    expiry_height: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct PreparedPaymentJournalRecord {
    activity_id: String,
    payment: PreparedPaymentRecord,
}

#[derive(Deserialize)]
struct AddressFaucetResult {
    address: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    status: String,
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn address_faucet_retry_after_server_restart_pays_each_destination_once() -> Result<()> {
    faucet::address_faucet_retry_after_server_restart_pays_each_destination_once().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn same_account_cross_pool_round_trip_is_replay_safe() -> Result<()> {
    send::same_account_cross_pool_round_trip_is_replay_safe().await
}

#[derive(Debug, Deserialize)]
struct FaucetAccount {
    id: u8,
    transparent_address: String,
    unified_address: String,
    transparent_zatoshi: u64,
    ironwood_zatoshi: u64,
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn internal_address_faucets_record_confirmed_activity() -> Result<()> {
    faucet::internal_address_faucets_record_confirmed_activity().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn internal_address_faucet_recovers_after_auto_mine_failure() -> Result<()> {
    faucet::internal_address_faucet_recovers_after_auto_mine_failure().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn external_address_faucet_behavior_is_unchanged() -> Result<()> {
    faucet::external_address_faucet_behavior_is_unchanged().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn prepare_rolls_back_constructed_transaction_when_journal_write_fails() -> Result<()> {
    recovery::prepare_rolls_back_constructed_transaction_when_journal_write_fails().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn concurrent_identical_sends_have_one_chain_effect() -> Result<()> {
    send::concurrent_identical_sends_have_one_chain_effect().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn broadcast_recovers_after_auto_mine_failure() -> Result<()> {
    recovery::broadcast_recovers_after_auto_mine_failure().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn large_reward_history_keeps_treasury_faucet_responsive() -> Result<()> {
    treasury::large_reward_history_keeps_treasury_faucet_responsive().await
}

async fn run_live_scenario(live_scenario: LiveScenario) -> Result<()> {
    let reporter = Arc::new(Mutex::new(RecoveryFailureReporter::new()));
    let mut signals = match TerminationSignals::install() {
        Ok(signals) => signals,
        Err(error) => {
            reporter.lock().await.emit(None, FailureRoute::Setup);
            return Err(error);
        }
    };

    let server = PathBuf::from(env!("CARGO_BIN_EXE_ths-server"));
    let fixture = match RegtestStack::new(server) {
        Ok(fixture) => Arc::new(Mutex::new(fixture)),
        Err(error) => {
            reporter.lock().await.emit(None, FailureRoute::Setup);
            return Err(error);
        }
    };

    let scenario_fixture = Arc::clone(&fixture);
    let scenario_reporter = Arc::clone(&reporter);
    let mut scenario = tokio::spawn(async move {
        let mut fixture = scenario_fixture.lock().await;
        let mut reporter = scenario_reporter.lock().await;
        fixture.start().await?;
        fixture.assert_running().await?;
        match live_scenario {
            LiveScenario::BroadcastRecovery => exercise_recovery(&mut fixture, &mut reporter).await,
            LiveScenario::TreasurySync => exercise_treasury_sync(&mut fixture, &mut reporter).await,
        }
    });

    let (scenario_result, route) = tokio::select! {
        joined = &mut scenario => match joined {
            Ok(result) => (result, FailureRoute::Error),
            Err(_) => (
                Err(anyhow::anyhow!("live recovery scenario task ended unexpectedly")),
                FailureRoute::Error,
            ),
        },
        _ = signals.cancelled() => {
            scenario.abort();
            let _ = scenario.await;
            (
                Err(anyhow::anyhow!("live recovery scenario interrupted")),
                FailureRoute::Signal,
            )
        },
    };

    if scenario_result.is_ok() {
        reporter.lock().await.phase(RecoveryPhase::Cleanup);
    }
    let cleanup_result = {
        let mut fixture = fixture.lock().await;
        fixture.shutdown().await
    };
    let cleanup_route = scenario_result.is_ok() && cleanup_result.is_err();
    let result = preserve_scenario_failure(scenario_result, cleanup_result);

    if result.is_err() {
        let fixture = fixture.lock().await;
        reporter.lock().await.emit(
            Some(&fixture),
            if cleanup_route {
                FailureRoute::Cleanup
            } else {
                route
            },
        );
    }
    result
}

#[cfg(test)]
#[path = "scenarios/activity_recovery/tests.rs"]
mod tests;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn startup_funding_retry_after_auto_mine_failure_has_one_payout() -> Result<()> {
    treasury::startup_funding_retry_after_auto_mine_failure_has_one_payout().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn startup_outage_serves_stored_status_and_recovers() -> Result<()> {
    recovery::startup_outage_serves_stored_status_and_recovers().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn external_address_send_progresses_without_caller_replay() -> Result<()> {
    send::external_address_send_progresses_without_caller_replay().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn disconnected_payment_preparation_keeps_the_original_transaction() -> Result<()> {
    send::disconnected_payment_preparation_keeps_the_original_transaction().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn expired_unseen_send_replaces_once_under_the_original_key() -> Result<()> {
    send::expired_unseen_send_replaces_once_under_the_original_key().await
}

struct PaymentAdmission {
    operation: serde_json::Value,
    location: String,
}

async fn admit_payment(
    client: &Client,
    base: &str,
    path: &str,
    body: &serde_json::Value,
) -> Result<PaymentAdmission> {
    let response = client
        .post(format!("{base}{path}"))
        .timeout(API_READ_TIMEOUT)
        .json(body)
        .send()
        .await?;
    let status = response.status();
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .context("payment admission omitted Location")?
        .to_str()?
        .to_owned();
    let operation: serde_json::Value = response.json().await?;
    let terminal = matches!(operation["status"].as_str(), Some("confirmed" | "failed"));
    anyhow::ensure!(
        status
            == if terminal {
                reqwest::StatusCode::OK
            } else {
                reqwest::StatusCode::ACCEPTED
            },
        "payment admission returned {status}: {operation}"
    );
    let expected_path = if path == "/api/v1/faucet/address" {
        "/api/v1/faucet/address"
    } else {
        "/api/v1/activity"
    };
    let base_url = reqwest::Url::parse(base)?;
    let url = base_url.join(&location)?;
    let query = url.query_pairs().collect::<Vec<_>>();
    anyhow::ensure!(
        location.starts_with('/')
            && !location.starts_with("//")
            && url.origin() == base_url.origin()
            && url.path() == expected_path
            && query.len() == 1
            && query[0].0 == "idempotency_key"
            && query[0].1
                == body["idempotency_key"]
                    .as_str()
                    .context("missing request key")?,
        "payment Location did not preserve the exact request key"
    );
    Ok(PaymentAdmission {
        operation,
        location,
    })
}

async fn wait_payment_state(
    client: &Client,
    base: &str,
    location: &str,
    status: &str,
    deadline: Instant,
) -> Result<serde_json::Value> {
    loop {
        let value: serde_json::Value =
            request_json(client, base, location, None, API_READ_TIMEOUT).await?;
        let operation = if location.starts_with("/api/v1/activity?") {
            let activities = value
                .as_array()
                .context("activity status is not an array")?;
            anyhow::ensure!(
                activities.len() == 1,
                "exact payment key did not identify one activity"
            );
            activities[0].clone()
        } else {
            value
        };
        anyhow::ensure!(
            operation["status"] != "failed",
            "payment failed: {operation}"
        );
        if operation["status"] == status
            && (status != "broadcast"
                || operation["last_error"]
                    .as_str()
                    .is_some_and(|error| error.contains("injected auto-mine failure")))
        {
            if status == "confirmed" {
                anyhow::ensure!(
                    operation["last_error"].is_null(),
                    "confirmation retained an execution error"
                );
            }
            return Ok(operation);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "payment did not reach {status}: {operation}"
        );
        tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
    }
}

async fn confirmed_payment<T: serde::de::DeserializeOwned>(
    client: &Client,
    base: &str,
    path: &str,
    body: Option<&serde_json::Value>,
    timeout: Duration,
) -> Result<T> {
    let admission =
        admit_payment(client, base, path, body.context("missing payment request")?).await?;
    let confirmed = wait_payment_state(
        client,
        base,
        &admission.location,
        "confirmed",
        Instant::now() + timeout,
    )
    .await?;
    if let Some(id) = admission.operation["id"].as_str() {
        anyhow::ensure!(
            confirmed["id"] == id,
            "confirmation changed admitted activity identity"
        );
    }
    if let Some(txid) = admission.operation["txid"]
        .as_str()
        .filter(|txid| !txid.is_empty())
    {
        anyhow::ensure!(
            confirmed["txid"] == txid,
            "confirmation replaced retained transaction identity"
        );
    }
    Ok(serde_json::from_value(confirmed)?)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn background_recovers_prepare_interrupted_before_store_recording() -> Result<()> {
    recovery::background_recovers_prepare_interrupted_before_store_recording().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn restart_recovers_claim_before_preparation_without_payment_post() -> Result<()> {
    recovery::restart_recovers_claim_before_preparation_without_payment_post().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn insufficient_send_stays_failed_after_restart_and_later_funding() -> Result<()> {
    recovery::insufficient_send_stays_failed_after_restart_and_later_funding().await
}
