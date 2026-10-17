use super::*;
use super::{
    API_READ_TIMEOUT, Activity, ChainInfo, Client, Context, Duration, Instant, PathBuf,
    RECOVERY_POLL_INTERVAL, RegtestStack, Result, SEND_TIMEOUT, admit_payment, json,
    preserve_scenario_failure, request_json, rpc,
};

/// How long a pending manual payment is observed before it is explicitly mined.
/// The window is longer than any automatic retry period so an unexpected block
/// would surface as a failure.
const MANUAL_PENDING_WINDOW: Duration = Duration::from_secs(6);

fn send_body(key: &str) -> serde_json::Value {
    json!({
        "from_account": 1, "to_account": 2,
        "source_pool": "ironwood", "destination_pool": "ironwood",
        "amount_zatoshi": 1_000_000, "idempotency_key": key,
    })
}

async fn node_height(client: &Client, fixture: &RegtestStack) -> Result<u64> {
    let chain: ChainInfo = rpc(client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
    Ok(chain.blocks)
}

async fn wait_for_height_growth(
    client: &Client,
    fixture: &RegtestStack,
    baseline: u64,
    deadline: Instant,
) -> Result<u64> {
    loop {
        let height = node_height(client, fixture).await?;
        if height > baseline {
            return Ok(height);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "interval mining did not advance the chain within its deadline"
        );
        tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
    }
}

async fn activity_status(client: &Client, base: &str, location: &str) -> Result<serde_json::Value> {
    let value: serde_json::Value =
        request_json(client, base, location, None, API_READ_TIMEOUT).await?;
    if location.starts_with("/api/v1/activity?") {
        let activities = value
            .as_array()
            .context("activity status is not an array")?;
        anyhow::ensure!(
            activities.len() == 1,
            "exact payment key did not identify one activity"
        );
        Ok(activities[0].clone())
    } else {
        Ok(value)
    }
}

async fn wait_for_activity_status(
    client: &Client,
    base: &str,
    location: &str,
    wanted: &[&str],
    deadline: Instant,
) -> Result<serde_json::Value> {
    loop {
        let operation = activity_status(client, base, location).await?;
        anyhow::ensure!(
            operation["status"] != "failed",
            "payment failed: {operation}"
        );
        if wanted.contains(&operation["status"].as_str().unwrap_or_default()) {
            return Ok(operation);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "payment did not reach {wanted:?}: {operation}"
        );
        tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
    }
}

pub(super) async fn interval_mode_mines_empty_blocks_without_api_actions() -> Result<()> {
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    fixture.configure_mining("interval", Some(1));
    let scenario = async {
        fixture.start().await?;
        fixture.assert_running().await?;
        let client = Client::new();
        let baseline = node_height(&client, &fixture).await?;
        let advanced =
            wait_for_height_growth(&client, &fixture, baseline, Instant::now() + SEND_TIMEOUT)
                .await?;
        anyhow::ensure!(
            advanced > baseline,
            "interval mode did not advance the chain without an API request"
        );
        fixture.assert_running().await?;
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

pub(super) async fn interval_mode_confirms_a_payment_without_ths_mine() -> Result<()> {
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    fixture.configure_mining("interval", Some(2));
    let scenario = async {
        fixture.start().await?;
        fixture.assert_running().await?;
        let client = Client::new();
        let body = send_body("interval-mode-confirms-send");
        let sent: Activity = confirmed_payment(
            &client,
            fixture.api_url(),
            "/api/v1/send",
            Some(&body),
            SEND_TIMEOUT,
        )
        .await?;
        anyhow::ensure!(
            sent.status == "confirmed",
            "interval payment did not confirm"
        );
        let tx: serde_json::Value = rpc(
            &client,
            fixture.node_url(),
            "getrawtransaction",
            json!([sent.txid, 1]),
        )
        .await?;
        anyhow::ensure!(
            tx["confirmations"].as_u64().is_some_and(|count| count > 0),
            "interval payment lacks a confirmation"
        );
        fixture.assert_running().await?;
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

pub(super) async fn mempool_mode_confirms_a_payment_without_ths_mine() -> Result<()> {
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    fixture.configure_mining("mempool", None);
    let scenario = async {
        fixture.start().await?;
        fixture.assert_running().await?;
        let client = Client::new();
        let body = send_body("mempool-mode-confirms-send");
        let sent: Activity = confirmed_payment(
            &client,
            fixture.api_url(),
            "/api/v1/send",
            Some(&body),
            SEND_TIMEOUT,
        )
        .await?;
        anyhow::ensure!(
            sent.status == "confirmed",
            "mempool payment did not confirm"
        );
        let tx: serde_json::Value = rpc(
            &client,
            fixture.node_url(),
            "getrawtransaction",
            json!([sent.txid, 1]),
        )
        .await?;
        anyhow::ensure!(
            tx["confirmations"].as_u64().is_some_and(|count| count > 0),
            "mempool payment lacks a confirmation"
        );
        fixture.assert_running().await?;
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

pub(super) async fn manual_mode_keeps_a_payment_pending_until_ths_mine() -> Result<()> {
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    fixture.configure_mining("manual", None);
    let scenario = async {
        fixture.start().await?;
        fixture.assert_running().await?;
        let client = Client::new();
        let body = send_body("manual-mode-pending-send");
        let admission = admit_payment(&client, fixture.api_url(), "/api/v1/send", &body).await?;
        let location = admission.location.clone();
        let broadcast = wait_for_activity_status(
            &client,
            fixture.api_url(),
            &location,
            &["broadcast"],
            Instant::now() + SEND_TIMEOUT,
        )
        .await?;
        anyhow::ensure!(
            broadcast["status"] == "broadcast",
            "manual payment did not reach the mempool"
        );

        // Nothing mines in manual mode, so the chain height must stay put.
        let pending_height = node_height(&client, &fixture).await?;
        tokio::time::sleep(MANUAL_PENDING_WINDOW).await;
        let settled_height = node_height(&client, &fixture).await?;
        anyhow::ensure!(
            settled_height == pending_height,
            "manual mode mined a block without an explicit request"
        );
        let still_pending = activity_status(&client, fixture.api_url(), &location).await?;
        anyhow::ensure!(
            still_pending["status"] == "broadcast",
            "manual payment changed state without a mine request"
        );

        let mined: serde_json::Value = client
            .post(format!("{}/api/v1/mine", fixture.api_url()))
            .timeout(API_READ_TIMEOUT + SEND_TIMEOUT)
            .json(&json!({"blocks": 1}))
            .send()
            .await?
            .json()
            .await?;
        anyhow::ensure!(mined["blocks"] == 1, "explicit mine did not mine one block");
        let confirmed = wait_for_activity_status(
            &client,
            fixture.api_url(),
            &location,
            &["confirmed"],
            Instant::now() + SEND_TIMEOUT,
        )
        .await?;
        anyhow::ensure!(
            confirmed["status"] == "confirmed",
            "explicit mine did not confirm the payment"
        );
        let txid = confirmed["txid"]
            .as_str()
            .context("confirmed manual payment omitted its txid")?;
        let tx: serde_json::Value = rpc(
            &client,
            fixture.node_url(),
            "getrawtransaction",
            json!([txid, 1]),
        )
        .await?;
        anyhow::ensure!(
            tx["confirmations"].as_u64().is_some_and(|count| count > 0),
            "manual payment lacks a confirmation after an explicit mine"
        );
        fixture.assert_running().await?;
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}
