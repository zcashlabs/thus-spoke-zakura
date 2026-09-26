mod support;

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use reqwest::Client;
use rusqlite::{Connection, OpenFlags, params};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use support::{
    FailureRoute, GenerateCounts, HeightCheckpoint, RecoveryFailureReporter, RecoveryPhase,
    RegtestStack, TerminationSignals, request_json, rpc,
};

const RECOVERY_IDEMPOTENCY_KEY: &str = "recovery-after-auto-mine-failure";
const CONCURRENT_IDEMPOTENCY_KEY: &str = "concurrent-identical-send";
const RECOVERY_POLL_INTERVAL: Duration = Duration::from_millis(250);
const API_READ_TIMEOUT: Duration = Duration::from_secs(5);
const SEND_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct Activity {
    id: String,
    kind: String,
    from_account: Option<u8>,
    to_account: u8,
    source_pool: String,
    destination_pool: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    status: String,
    created_at: String,
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
    orchard_zatoshi: u64,
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn concurrent_identical_sends_have_one_chain_effect() -> Result<()> {
    let server = PathBuf::from(env!("CARGO_BIN_EXE_tsz-server"));
    let mut fixture = RegtestStack::new(server)?;
    let scenario = async {
        fixture.start().await?;
        fixture.assert_running().await?;
        let client = Client::new();
        let before_accounts: Vec<AccountBalance> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/accounts",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let before_balance = before_accounts
            .iter()
            .find(|account| account.id == 2)
            .map(|account| account.orchard_zatoshi)
            .ok_or_else(|| anyhow::anyhow!("destination account is missing"))?;
        let before_activities: Vec<Activity> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/activity?limit=100",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let request = json!({
            "from_account": 1,
            "to_account": 2,
            "source_pool": "orchard",
            "destination_pool": "orchard",
            "amount_zatoshi": 10_000_000,
            "idempotency_key": CONCURRENT_IDEMPOTENCY_KEY,
        });

        let (first, second) = tokio::join!(
            request_json::<Activity>(
                &client,
                fixture.api_url(),
                "/api/v1/send",
                Some(&request),
                SEND_TIMEOUT,
            ),
            request_json::<Activity>(
                &client,
                fixture.api_url(),
                "/api/v1/send",
                Some(&request),
                SEND_TIMEOUT,
            ),
        );
        let first = first?;
        let second = second?;
        anyhow::ensure!(
            first.id == second.id,
            "requests returned different activities"
        );
        anyhow::ensure!(
            first.txid == second.txid,
            "requests returned different transactions"
        );
        anyhow::ensure!(
            first.status == "confirmed" && second.status == "confirmed",
            "requests did not converge on a confirmed payment"
        );

        let after_accounts: Vec<AccountBalance> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/accounts",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let after_balance = after_accounts
            .iter()
            .find(|account| account.id == 2)
            .map(|account| account.orchard_zatoshi)
            .ok_or_else(|| anyhow::anyhow!("destination account is missing"))?;
        anyhow::ensure!(
            after_balance.checked_sub(before_balance) == Some(10_000_000),
            "recipient balance changed by more than one payment"
        );
        let after_activities: Vec<Activity> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/activity?limit=100",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        anyhow::ensure!(
            after_activities.len() == before_activities.len() + 1,
            "concurrent requests did not create exactly one activity"
        );
        anyhow::ensure!(
            after_activities
                .iter()
                .filter(|activity| activity.id == first.id && activity.txid == first.txid)
                .count()
                == 1,
            "activity and transaction were not recorded exactly once"
        );
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn broadcast_recovers_after_auto_mine_failure() -> Result<()> {
    let reporter = Arc::new(Mutex::new(RecoveryFailureReporter::new()));
    let mut signals = match TerminationSignals::install() {
        Ok(signals) => signals,
        Err(error) => {
            reporter.lock().await.emit(None, FailureRoute::Setup);
            return Err(error);
        }
    };

    let server = PathBuf::from(env!("CARGO_BIN_EXE_tsz-server"));
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
        exercise_recovery(&mut fixture, &mut reporter).await
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

async fn exercise_recovery(
    fixture: &mut RegtestStack,
    reporter: &mut RecoveryFailureReporter,
) -> Result<()> {
    let client = Client::new();
    reporter.phase(RecoveryPhase::Broadcast);

    let initial_activities: Vec<Activity> = request_json(
        &client,
        fixture.api_url(),
        "/api/v1/activity?limit=100",
        None,
        API_READ_TIMEOUT,
    )
    .await?;
    let before_auto_mine: ChainInfo =
        rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
    reporter.record_height(HeightCheckpoint::BeforeAutoMine, before_auto_mine.blocks);
    let proxy_before = fixture.proxy().counts();

    reporter.phase(RecoveryPhase::AutoMine);
    fixture.proxy().fail_next_generate()?;
    let sent: Activity = request_json(
        &client,
        fixture.api_url(),
        "/api/v1/send",
        Some(&json!({
            "from_account": 1,
            "to_account": 2,
            "source_pool": "orchard",
            "destination_pool": "orchard",
            "amount_zatoshi": 1000000,
            "idempotency_key": RECOVERY_IDEMPOTENCY_KEY,
        })),
        SEND_TIMEOUT,
    )
    .await?;
    assert_requested_payment_fields(&sent)?;
    anyhow::ensure!(
        sent.status == "broadcast",
        "send returned an activity that was not broadcast"
    );
    anyhow::ensure!(
        sent.block_hash.is_none(),
        "send returned an activity with an unexpected block hash"
    );
    reporter.record_activity(&sent.id, &sent.status, &sent.txid);

    let proxy_after_auto_mine = fixture.proxy().counts();
    anyhow::ensure!(
        proxy_after_auto_mine
            == GenerateCounts {
                rejected: proxy_before.rejected + 1,
                forwarded: proxy_before.forwarded,
            },
        "the injected auto-mine fault did not reject exactly one generate request"
    );
    let after_auto_mine: ChainInfo =
        rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
    reporter.record_height(HeightCheckpoint::AfterAutoMine, after_auto_mine.blocks);
    anyhow::ensure!(
        after_auto_mine.blocks == before_auto_mine.blocks,
        "auto-mine failure changed the node height"
    );
    let mempool: Vec<String> = rpc(&client, fixture.node_url(), "getrawmempool", json!([])).await?;
    anyhow::ensure!(
        mempool.iter().any(|txid| txid == &sent.txid),
        "broadcast transaction was not present in the node mempool"
    );

    let persisted_activities: Vec<Activity> = request_json(
        &client,
        fixture.api_url(),
        "/api/v1/activity?limit=100",
        None,
        API_READ_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(
        persisted_activities.len() == initial_activities.len() + 1,
        "broadcast did not add exactly one activity row"
    );
    let persisted_broadcast = one_matching_activity(&persisted_activities, &sent)?;
    assert_persisted_send_response(&sent, &persisted_broadcast)?;

    reporter.phase(RecoveryPhase::DirectMine);
    let _: Vec<String> = rpc(&client, fixture.node_url(), "generate", json!([1])).await?;
    let inclusion_tip: ChainInfo =
        rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
    reporter.record_height(HeightCheckpoint::Inclusion, inclusion_tip.blocks);
    let inclusion: TransactionEvidence = rpc(
        &client,
        fixture.node_url(),
        "getrawtransaction",
        json!([sent.txid, 1]),
    )
    .await?;
    anyhow::ensure!(
        inclusion
            .confirmations
            .is_some_and(|confirmations| confirmations >= 1),
        "directly mined transaction does not have a confirmation"
    );
    let expected_block_hash = inclusion
        .blockhash
        .filter(|hash| !hash.is_empty())
        .ok_or_else(|| anyhow::anyhow!("included transaction did not report a block hash"))?;

    let _: Vec<String> = rpc(&client, fixture.node_url(), "generate", json!([1])).await?;
    let later_tip: ChainInfo =
        rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
    reporter.record_height(HeightCheckpoint::Tip, later_tip.blocks);
    anyhow::ensure!(
        later_tip.bestblockhash != expected_block_hash,
        "the later chain tip did not differ from the transaction inclusion block"
    );

    reporter.phase(RecoveryPhase::Recovery);
    let recovery_deadline = RegtestStack::recovery_deadline();
    let confirmed = loop {
        let status = match fixture
            .recovery_read::<Status>(recovery_deadline, "/api/v1/status")
            .await
        {
            Ok(status) => status,
            Err(error) if support::is_retryable_read_transport(&error) => {
                wait_for_recovery_poll(recovery_deadline).await?;
                continue;
            }
            Err(error) => return Err(error),
        };
        let activities = match fixture
            .recovery_read::<Vec<Activity>>(recovery_deadline, "/api/v1/activity?limit=100")
            .await
        {
            Ok(activities) => activities,
            Err(error) if support::is_retryable_read_transport(&error) => {
                wait_for_recovery_poll(recovery_deadline).await?;
                continue;
            }
            Err(error) => return Err(error),
        };

        if let Some(height) = status.wallet_sync.fully_scanned_height {
            reporter.record_height(HeightCheckpoint::Scanned, height);
        }
        let activity = one_matching_activity(&activities, &persisted_broadcast)?;
        if status.wallet_sync.state == "ready"
            && status
                .wallet_sync
                .fully_scanned_height
                .is_some_and(|height| height >= later_tip.blocks)
            && activity.status == "confirmed"
        {
            break activity;
        }
        wait_for_recovery_poll(recovery_deadline).await?;
    };

    reporter.record_activity(&confirmed.id, &confirmed.status, &confirmed.txid);
    let mut expected_confirmed = persisted_broadcast.clone();
    expected_confirmed.status = "confirmed".to_owned();
    expected_confirmed.block_hash = Some(expected_block_hash.clone());
    anyhow::ensure!(
        confirmed == expected_confirmed,
        "activity changed beyond its confirmation status and inclusion block hash"
    );
    assert_requested_payment_fields(&confirmed)?;
    anyhow::ensure!(
        confirmed.status == "confirmed",
        "recovered activity was not confirmed"
    );
    anyhow::ensure!(
        fixture.proxy().counts() == proxy_after_auto_mine,
        "recovery polling changed proxy mining counters"
    );
    let final_activities: Vec<Activity> = request_json(
        &client,
        fixture.api_url(),
        "/api/v1/activity?limit=100",
        None,
        API_READ_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(
        final_activities.len() == initial_activities.len() + 1,
        "recovery changed the activity count"
    );
    let final_activity = one_matching_activity(&final_activities, &persisted_broadcast)?;
    anyhow::ensure!(
        final_activity == confirmed,
        "final activity read did not preserve the recovered row"
    );

    assert_read_only_persistence(
        fixture.data_dir().join("tsz.db"),
        &persisted_broadcast,
        &expected_block_hash,
    )?;
    Ok(())
}

fn assert_requested_payment_fields(activity: &Activity) -> Result<()> {
    anyhow::ensure!(activity.kind == "send", "activity kind was not send");
    anyhow::ensure!(
        activity.from_account == Some(1),
        "activity source account changed"
    );
    anyhow::ensure!(
        activity.to_account == 2,
        "activity destination account changed"
    );
    anyhow::ensure!(
        activity.source_pool == "orchard",
        "activity source pool changed"
    );
    anyhow::ensure!(
        activity.destination_pool == "orchard",
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

fn one_matching_activity(activities: &[Activity], expected: &Activity) -> Result<Activity> {
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

fn assert_persisted_send_response(immediate: &Activity, persisted: &Activity) -> Result<()> {
    anyhow::ensure!(
        !persisted.created_at.is_empty(),
        "persisted activity timestamp was empty"
    );
    let mut expected = immediate.clone();
    expected.created_at = persisted.created_at.clone();
    anyhow::ensure!(
        persisted == &expected,
        "persisted activity changed beyond its generated timestamp"
    );
    Ok(())
}

async fn wait_for_recovery_poll(deadline: Instant) -> Result<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    anyhow::ensure!(
        !remaining.is_zero(),
        "background activity recovery exceeded its 120-second deadline"
    );
    tokio::time::sleep(remaining.min(RECOVERY_POLL_INTERVAL)).await;
    Ok(())
}

fn assert_read_only_persistence(
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

fn preserve_scenario_failure<T>(scenario: Result<T>, cleanup: Result<()>) -> Result<T> {
    match (scenario, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), _) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::{future::pending, sync::Arc};

    use anyhow::Result;
    use tokio::sync::{Mutex, oneshot};

    use super::{Activity, assert_persisted_send_response, preserve_scenario_failure};

    fn activity(created_at: &str) -> Activity {
        Activity {
            id: "9b6a7ddd-8723-4163-b99e-7b16d62efef5".to_owned(),
            kind: "send".to_owned(),
            from_account: Some(1),
            to_account: 2,
            source_pool: "orchard".to_owned(),
            destination_pool: "orchard".to_owned(),
            amount_zatoshi: 1_000_000,
            txid: "b".repeat(64),
            block_hash: None,
            status: "broadcast".to_owned(),
            created_at: created_at.to_owned(),
        }
    }

    #[test]
    fn timestamp_normalization_excludes_only_the_immediate_send_timestamp() -> Result<()> {
        let immediate = activity("");
        let persisted = activity("2026-09-23 12:00:00");
        assert_persisted_send_response(&immediate, &persisted)?;

        let mut changed_amount = persisted.clone();
        changed_amount.amount_zatoshi += 1;
        assert!(assert_persisted_send_response(&immediate, &changed_amount).is_err());

        let mut confirmed = persisted.clone();
        confirmed.status = "confirmed".to_owned();
        confirmed.block_hash = Some("c".repeat(64));
        let mut expected = persisted.clone();
        expected.status = "confirmed".to_owned();
        expected.block_hash = Some("c".repeat(64));
        assert_eq!(confirmed, expected);

        confirmed.created_at = "2026-09-23 12:00:01".to_owned();
        assert_ne!(confirmed, expected);
        Ok(())
    }

    #[test]
    fn cleanup_error_does_not_mask_a_scenario_error() {
        let result = preserve_scenario_failure::<()>(
            Err(anyhow::anyhow!("scenario failure marker")),
            Err(anyhow::anyhow!("cleanup failure marker")),
        );
        let error = result.expect_err("scenario failure must remain the result");
        assert!(error.to_string().contains("scenario failure marker"));
        assert!(!error.to_string().contains("cleanup failure marker"));
    }

    #[tokio::test]
    async fn test_local_cancellation_aborts_awaits_then_records_cleanup() -> Result<()> {
        let (cancel_sender, mut cancel_receiver) = oneshot::channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut scenario = tokio::spawn(async {
            pending::<()>().await;
            Ok::<(), anyhow::Error>(())
        });
        cancel_sender
            .send(())
            .map_err(|_| anyhow::anyhow!("test-local cancellation receiver was unavailable"))?;

        tokio::select! {
            _ = &mut cancel_receiver => {
                scenario.abort();
                assert!(scenario.await.is_err());
                events.lock().await.push("scenario-aborted-and-awaited");
            }
            _ = &mut scenario => panic!("test scenario completed before cancellation"),
        }
        events.lock().await.push("cleanup-recorded");
        assert_eq!(
            events.lock().await.as_slice(),
            ["scenario-aborted-and-awaited", "cleanup-recorded"]
        );
        Ok(())
    }
}
