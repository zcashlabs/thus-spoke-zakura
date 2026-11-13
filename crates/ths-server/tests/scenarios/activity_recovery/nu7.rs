use super::*;

/// Startup funding completes near height 105, so NU7 at 150 leaves room for a
/// ready instance and a payment before activation.
const NU7_HEIGHT: u64 = 150;
const NU6_3_BRANCH_ID: u32 = 0x37a5_165b;
const NU7_BRANCH_ID: u32 = 0x7719_0ad9;
const STARTUP_FUNDING_KEY: &str = "startup-account-1-ironwood-v1";
const STARTUP_FUNDING_ZATOSHI: u64 = 500_000_000;

fn send_body(key: &str) -> serde_json::Value {
    json!({
        "from_account": 1, "to_account": 2,
        "source_pool": "ironwood", "destination_pool": "ironwood",
        "amount_zatoshi": 1_000_000, "idempotency_key": key,
    })
}

async fn chain(client: &Client, fixture: &RegtestStack) -> Result<serde_json::Value> {
    rpc(client, fixture.node_url(), "getblockchaininfo", json!([])).await
}

fn tip_height(chain: &serde_json::Value) -> Result<u64> {
    chain["blocks"]
        .as_u64()
        .context("getblockchaininfo omitted its height")
}

/// The activation height and status Zakura reports for one network upgrade.
fn upgrade(chain: &serde_json::Value, name: &str) -> Result<(u64, String)> {
    let upgrade = chain["upgrades"]
        .as_object()
        .context("getblockchaininfo omitted its upgrades")?
        .values()
        .find(|upgrade| upgrade["name"] == name)
        .with_context(|| format!("getblockchaininfo omitted {name}"))?;
    Ok((
        upgrade["activationheight"]
            .as_u64()
            .with_context(|| format!("{name} omitted its activation height"))?,
        upgrade["status"]
            .as_str()
            .with_context(|| format!("{name} omitted its status"))?
            .to_owned(),
    ))
}

/// The consensus branch ID a transaction was signed for. Version 5 and 6
/// transactions carry it in header bytes 8 to 12, little-endian.
async fn branch_id(client: &Client, fixture: &RegtestStack, txid: &str) -> Result<u32> {
    let raw: String = rpc(
        client,
        fixture.node_url(),
        "getrawtransaction",
        json!([txid, 0]),
    )
    .await?;
    let bytes = hex::decode(raw).context("decoding raw transaction")?;
    anyhow::ensure!(bytes.len() >= 12, "raw transaction {txid} is truncated");
    Ok(u32::from_le_bytes(bytes[8..12].try_into()?))
}

async fn coinbase_branch_id(client: &Client, fixture: &RegtestStack, height: u64) -> Result<u32> {
    let block: serde_json::Value = rpc(
        client,
        fixture.node_url(),
        "getblock",
        json!([height.to_string(), 1]),
    )
    .await?;
    let coinbase = block["tx"][0]
        .as_str()
        .with_context(|| format!("block {height} omitted its coinbase"))?;
    branch_id(client, fixture, coinbase).await
}

async fn block_height(client: &Client, fixture: &RegtestStack, hash: &str) -> Result<u64> {
    let block: serde_json::Value =
        rpc(client, fixture.node_url(), "getblock", json!([hash, 1])).await?;
    block["height"]
        .as_u64()
        .with_context(|| format!("block {hash} omitted its height"))
}

async fn mine_to(client: &Client, fixture: &RegtestStack, height: u64) -> Result<()> {
    let tip = tip_height(&chain(client, fixture).await?)?;
    anyhow::ensure!(
        tip < height,
        "chain is already at {tip}, not below {height}"
    );
    let blocks = height - tip;
    let mined: serde_json::Value = request_json(
        client,
        fixture.api_url(),
        "/api/v1/mine",
        Some(&json!({"blocks": blocks})),
        API_READ_TIMEOUT + SEND_TIMEOUT,
    )
    .await
    .with_context(|| format!("mining from {tip} to {height}"))?;
    anyhow::ensure!(mined["blocks"] == blocks, "mining returned {mined}");
    Ok(())
}

/// Waits until the node, lightwalletd, and wallet all report `height` as the
/// settled tip, which is what `ths wait` reports.
async fn wait_settled_at(
    client: &Client,
    fixture: &RegtestStack,
    height: u64,
) -> Result<serde_json::Value> {
    let deadline = Instant::now() + SEND_TIMEOUT;
    loop {
        let status: serde_json::Value = request_json(
            client,
            fixture.api_url(),
            "/api/v1/status",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        if status["settled"] == true
            && status["node_height"] == height
            && status["lightwalletd_height"] == height
            && status["wallet_scanned_height"] == height
        {
            return Ok(status);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "instance did not settle at {height}: {status}"
        );
        tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
    }
}

async fn confirmed_send(client: &Client, fixture: &RegtestStack, key: &str) -> Result<Activity> {
    let sent: Activity = confirmed_payment(
        client,
        fixture.api_url(),
        "/api/v1/send",
        Some(&send_body(key)),
        SEND_TIMEOUT,
    )
    .await
    .with_context(|| format!("sending {key}"))?;
    anyhow::ensure!(sent.status == "confirmed", "{key} did not confirm");
    Ok(sent)
}

/// Startup funds Account 1 once. A second payout would add another five ZEC to
/// the user accounts, which payments between them and their fees never do.
async fn assert_one_startup_funding(client: &Client, fixture: &RegtestStack) -> Result<()> {
    let funding: Vec<Activity> = request_json(
        client,
        fixture.api_url(),
        &format!("/api/v1/activity?idempotency_key={STARTUP_FUNDING_KEY}"),
        None,
        API_READ_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(
        funding.len() == 1
            && funding[0].status == "confirmed"
            && funding[0].to_account == Some(1)
            && funding[0].amount_zatoshi == STARTUP_FUNDING_ZATOSHI,
        "startup funding is not one confirmed payout: {funding:?}"
    );
    let accounts: Vec<serde_json::Value> = request_json(
        client,
        fixture.api_url(),
        "/api/v1/accounts",
        None,
        API_READ_TIMEOUT,
    )
    .await?;
    let mut total = 0;
    for account in &accounts {
        for pool in ["ironwood_zatoshi", "transparent_zatoshi"] {
            total += account[pool]
                .as_u64()
                .with_context(|| format!("account omitted {pool}: {account}"))?;
        }
    }
    anyhow::ensure!(
        total <= STARTUP_FUNDING_ZATOSHI,
        "user accounts hold {total} zatoshi, more than one startup funding: {accounts:?}"
    );
    Ok(())
}

pub(super) async fn delayed_nu7_activation_is_ready_below_activation_and_crosses_by_manual_mining()
-> Result<()> {
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    fixture.configure_nu7_activation_height(NU7_HEIGHT as u32);
    let scenario = async {
        // `start` waits for a ready wallet whose Account 1 holds the 5 startup ZEC
        // in Ironwood, so reaching here proves funding finished before NU7.
        fixture.start().await?;
        let client = Client::new();
        let ready = chain(&client, &fixture).await?;
        let ready_height = tip_height(&ready)?;
        anyhow::ensure!(
            ready_height + 2 < NU7_HEIGHT,
            "the instance became ready at {ready_height}, too close to NU7 at {NU7_HEIGHT}"
        );
        anyhow::ensure!(
            upgrade(&ready, "NU6.3")? == (1, "active".into())
                && upgrade(&ready, "NU7")? == (NU7_HEIGHT, "pending".into())
                && ready["consensus"]["chaintip"] == format!("{NU6_3_BRANCH_ID:08x}"),
            "Zakura does not run NU6.3 with NU7 pending at {NU7_HEIGHT}: {}",
            ready["upgrades"]
        );
        let status = wait_settled_at(&client, &fixture, ready_height).await?;
        anyhow::ensure!(
            status["nu7_activation_height"] == NU7_HEIGHT,
            "status reports another NU7 height: {status}"
        );

        let before = confirmed_send(&client, &fixture, "nu7-before-activation").await?;
        let before_height = block_height(
            &client,
            &fixture,
            before
                .block_hash
                .as_deref()
                .context("payment omitted its block")?,
        )
        .await?;
        anyhow::ensure!(
            before_height < NU7_HEIGHT
                && branch_id(&client, &fixture, &before.txid).await? == NU6_3_BRANCH_ID,
            "the pre-activation payment was not an NU6.3 transaction below {NU7_HEIGHT}"
        );

        mine_to(&client, &fixture, NU7_HEIGHT - 1).await?;
        wait_settled_at(&client, &fixture, NU7_HEIGHT - 1).await?;
        let last_nu6_3 = chain(&client, &fixture).await?;
        anyhow::ensure!(
            last_nu6_3["consensus"]["chaintip"] == format!("{NU6_3_BRANCH_ID:08x}")
                && last_nu6_3["consensus"]["nextblock"] == format!("{NU7_BRANCH_ID:08x}")
                && coinbase_branch_id(&client, &fixture, NU7_HEIGHT - 1).await? == NU6_3_BRANCH_ID,
            "block {} is not the last NU6.3 block: {}",
            NU7_HEIGHT - 1,
            last_nu6_3["consensus"]
        );

        mine_to(&client, &fixture, NU7_HEIGHT).await?;
        wait_settled_at(&client, &fixture, NU7_HEIGHT).await?;
        let activated = chain(&client, &fixture).await?;
        anyhow::ensure!(
            activated["consensus"]["chaintip"] == format!("{NU7_BRANCH_ID:08x}")
                && upgrade(&activated, "NU7")? == (NU7_HEIGHT, "active".into())
                && coinbase_branch_id(&client, &fixture, NU7_HEIGHT).await? == NU7_BRANCH_ID,
            "NU7 did not activate at {NU7_HEIGHT}: {}",
            activated["consensus"]
        );

        // Account 1 now spends change created before activation.
        let after = confirmed_send(&client, &fixture, "nu7-after-activation").await?;
        anyhow::ensure!(
            branch_id(&client, &fixture, &after.txid).await? == NU7_BRANCH_ID,
            "the post-activation payment was not an NU7 transaction"
        );
        assert_one_startup_funding(&client, &fixture).await?;
        fixture.assert_running().await?;
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

pub(super) async fn delayed_nu7_activation_survives_restart_rollback_and_reorg_without_duplicate_funding()
-> Result<()> {
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    fixture.configure_nu7_activation_height(NU7_HEIGHT as u32);
    let scenario = async {
        fixture.start().await?;
        let client = Client::new();
        mine_to(&client, &fixture, NU7_HEIGHT + 1).await?;
        wait_settled_at(&client, &fixture, NU7_HEIGHT + 1).await?;

        // The restarted server receives no height; it must read the stored one.
        fixture.restart_server().await?;
        let status = wait_settled_at(&client, &fixture, NU7_HEIGHT + 1).await?;
        anyhow::ensure!(
            status["nu7_activation_height"] == NU7_HEIGHT,
            "the restarted server lost the NU7 height: {status}"
        );

        let rollback: serde_json::Value = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/rollback",
            Some(&json!({"depth": 3})),
            SEND_TIMEOUT,
        )
        .await
        .context("rolling back below NU7 activation")?;
        anyhow::ensure!(
            rollback["new_tip"]["height"] == NU7_HEIGHT - 2,
            "rollback did not land below activation: {rollback}"
        );
        wait_settled_at(&client, &fixture, NU7_HEIGHT - 2).await?;
        let below = chain(&client, &fixture).await?;
        anyhow::ensure!(
            below["consensus"]["chaintip"] == format!("{NU6_3_BRANCH_ID:08x}")
                && upgrade(&below, "NU7")? == (NU7_HEIGHT, "pending".into()),
            "rollback below {NU7_HEIGHT} did not return to NU6.3: {}",
            below["consensus"]
        );

        // A payment built after this restart targets a pre-activation height. The
        // node rejects it unless the server still uses the stored NU7 height.
        fixture.restart_server().await?;
        wait_settled_at(&client, &fixture, NU7_HEIGHT - 2).await?;
        let sent = confirmed_send(&client, &fixture, "nu7-after-restart-below-activation").await?;
        anyhow::ensure!(
            branch_id(&client, &fixture, &sent.txid).await? == NU6_3_BRANCH_ID,
            "the payment after restart was not an NU6.3 transaction"
        );

        mine_to(&client, &fixture, NU7_HEIGHT + 2).await?;
        wait_settled_at(&client, &fixture, NU7_HEIGHT + 2).await?;
        // Replace NU7_HEIGHT..=NU7_HEIGHT + 2 so the new branch crosses activation.
        let reorg: serde_json::Value = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/reorg",
            Some(&json!({"depth": 3})),
            SEND_TIMEOUT,
        )
        .await
        .context("reorganizing across NU7 activation")?;
        anyhow::ensure!(
            reorg["replacement_hashes"]
                .as_array()
                .is_some_and(|hashes| hashes.len() == 3),
            "reorg did not mine three replacement blocks: {reorg}"
        );
        wait_settled_at(&client, &fixture, NU7_HEIGHT + 2).await?;
        anyhow::ensure!(
            coinbase_branch_id(&client, &fixture, NU7_HEIGHT - 1).await? == NU6_3_BRANCH_ID
                && coinbase_branch_id(&client, &fixture, NU7_HEIGHT).await? == NU7_BRANCH_ID,
            "the replacement branch did not activate NU7 at {NU7_HEIGHT}"
        );
        let crossed = chain(&client, &fixture).await?;
        anyhow::ensure!(
            upgrade(&crossed, "NU7")? == (NU7_HEIGHT, "active".into()),
            "Zakura changed the NU7 schedule: {}",
            crossed["upgrades"]
        );
        assert_one_startup_funding(&client, &fixture).await?;
        fixture.assert_running().await?;
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}
