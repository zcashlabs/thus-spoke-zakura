use super::*;
use super::{
    API_READ_TIMEOUT, AccountBalance, Activity, CONCURRENT_IDEMPOTENCY_KEY, ChainInfo, Client,
    PathBuf, RegtestStack, Result, SEND_TIMEOUT, assert_persisted_send_response, json,
    one_matching_activity, preserve_scenario_failure, queued_retry_survives_restart, request_json,
    rpc,
};

pub(super) async fn queued_send_retry_survives_lost_response_and_restart() -> Result<()> {
    queued_retry_survives_restart(false).await
}

pub(super) async fn same_account_cross_pool_round_trip_is_replay_safe() -> Result<()> {
    let server = PathBuf::from(env!("CARGO_BIN_EXE_ths-server"));
    let mut fixture = RegtestStack::new(server)?;
    let scenario = async {
        fixture.start().await?;
        fixture.assert_running().await?;
        let client = Client::new();
        let initial: Vec<AccountBalance> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/accounts",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let initial = initial
            .iter()
            .find(|a| a.id == 1)
            .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?;
        anyhow::ensure!(
            initial.transparent_zatoshi == 0,
            "initial transparent balance is nonzero"
        );
        let mut expected_ironwood = initial.ironwood_zatoshi;
        let initial_activity: Vec<Activity> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/activity?limit=100",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let mut prior_txid = None::<String>;
        for (index, source, destination, amount, key) in [
            (
                1,
                "ironwood",
                "transparent",
                1_000_000u64,
                "self-unshield-round-trip",
            ),
            (
                2,
                "transparent",
                "ironwood",
                500_000u64,
                "self-shield-round-trip",
            ),
        ] {
            let body = json!({
                "from_account": 1, "to_account": 1, "source_pool": source,
                "destination_pool": destination, "amount_zatoshi": amount,
                "idempotency_key": key,
            });
            let sent: Activity = confirmed_payment(
                &client,
                fixture.api_url(),
                "/api/v1/send",
                Some(&body),
                SEND_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(sent.status == "confirmed", "send did not confirm");
            anyhow::ensure!(sent.from_account == Some(1), "source account changed");
            anyhow::ensure!(sent.to_account == Some(1), "destination account changed");
            anyhow::ensure!(sent.source_pool == source, "source pool changed");
            anyhow::ensure!(
                sent.destination_pool == destination,
                "destination pool changed"
            );
            anyhow::ensure!(sent.amount_zatoshi == amount, "send amount changed");
            let tx: serde_json::Value = rpc(
                &client,
                fixture.node_url(),
                "getrawtransaction",
                json!([sent.txid, 1]),
            )
            .await?;
            anyhow::ensure!(
                tx["confirmations"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?
                    >= 1,
                "transaction has no confirmation"
            );
            let hash = tx["blockhash"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?;
            anyhow::ensure!(
                sent.block_hash.as_deref() == Some(hash),
                "activity block hash differs from the transaction"
            );
            let block: serde_json::Value =
                rpc(&client, fixture.node_url(), "getblock", json!([hash, 1])).await?;
            anyhow::ensure!(
                block["tx"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?
                    .iter()
                    .any(|id| id == &json!(sent.txid)),
                "transaction is absent from its confirming block"
            );
            let expected_transparent;
            if source == "ironwood" {
                anyhow::ensure!(
                    tx["vin"]
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?
                        .is_empty(),
                    "unshield transaction has transparent inputs"
                );
                anyhow::ensure!(
                    tx["vout"][0]["valueZat"] == 1_000_000,
                    "unshield transparent output differs from 1,000,000 zatoshis"
                );
                anyhow::ensure!(
                    tx["ironwood"]["valueBalanceZat"] == 1_015_000,
                    "unshield Ironwood balance differs from 1,015,000 zatoshis"
                );
                expected_ironwood -= 1_015_000;
                expected_transparent = 1_000_000;
                prior_txid = Some(sent.txid.clone());
            } else {
                anyhow::ensure!(
                    tx["vin"][0]["txid"].as_str() == prior_txid.as_deref(),
                    "shield does not consume the preceding unshield transaction"
                );
                anyhow::ensure!(
                    tx["vin"][0]["vout"] == 0,
                    "shield does not consume transparent output zero"
                );
                anyhow::ensure!(
                    tx["vout"]
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?
                        .is_empty(),
                    "shield left transparent change"
                );
                anyhow::ensure!(
                    tx["ironwood"]["valueBalanceZat"] == -985_000,
                    "shield Ironwood balance differs from -985,000 zatoshis"
                );
                expected_ironwood += 985_000;
                expected_transparent = 0;
            }
            let accounts: Vec<AccountBalance> = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/accounts",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            let account = accounts
                .iter()
                .find(|a| a.id == 1)
                .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?;
            anyhow::ensure!(
                account.ironwood_zatoshi == expected_ironwood,
                "Ironwood balance differs from the expected change"
            );
            anyhow::ensure!(
                account.transparent_zatoshi == expected_transparent,
                "transparent balance differs from the expected output"
            );
            let activities: Vec<Activity> = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/activity?limit=100",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(
                activities.len() == initial_activity.len() + index,
                "send did not add exactly one activity"
            );
            anyhow::ensure!(
                activities.iter().filter(|a| a.id == sent.id).count() == 1,
                "activity ID was not recorded exactly once"
            );
            anyhow::ensure!(
                activities.iter().filter(|a| a.txid == sent.txid).count() == 1,
                "transaction ID was not recorded exactly once"
            );
            let persisted = one_matching_activity(&activities, &sent)?;
            assert_persisted_send_response(&sent, &persisted)?;
            let height_before: ChainInfo =
                rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
            let replay: Activity = confirmed_payment(
                &client,
                fixture.api_url(),
                "/api/v1/send",
                Some(&body),
                SEND_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(
                replay == persisted,
                "replay differs from persisted activity"
            );
            let height_after: ChainInfo =
                rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
            anyhow::ensure!(
                height_after.blocks == height_before.blocks,
                "replay advanced the chain height"
            );
            anyhow::ensure!(
                height_after.bestblockhash == height_before.bestblockhash,
                "replay changed the chain tip"
            );
            let after: Vec<AccountBalance> = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/accounts",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            let after = after
                .iter()
                .find(|a| a.id == 1)
                .ok_or_else(|| anyhow::anyhow!("required round-trip evidence is missing"))?;
            anyhow::ensure!(
                after.ironwood_zatoshi == account.ironwood_zatoshi,
                "replay changed the Ironwood balance"
            );
            anyhow::ensure!(
                after.transparent_zatoshi == account.transparent_zatoshi,
                "replay changed the transparent balance"
            );
            let after_activity: Vec<Activity> = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/activity?limit=100",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(
                after_activity == activities,
                "replay changed activity history"
            );
        }
        anyhow::ensure!(
            initial.ironwood_zatoshi - expected_ironwood == 30_000,
            "round trip did not spend exactly 30,000 zatoshis in fees"
        );
        fixture.assert_running().await?;
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

pub(super) async fn concurrent_identical_sends_have_one_chain_effect() -> Result<()> {
    let server = PathBuf::from(env!("CARGO_BIN_EXE_ths-server"));
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
            .map(|account| account.ironwood_zatoshi)
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
            "source_pool": "ironwood",
            "destination_pool": "ironwood",
            "amount_zatoshi": 10_000_000,
            "idempotency_key": CONCURRENT_IDEMPOTENCY_KEY,
        });

        let (first, second) = tokio::join!(
            admit_payment(&client, fixture.api_url(), "/api/v1/send", &request),
            admit_payment(&client, fixture.api_url(), "/api/v1/send", &request),
        );
        let first = first?;
        let second = second?;
        anyhow::ensure!(
            first.operation["id"] == second.operation["id"],
            "concurrent admissions returned different activity IDs"
        );
        let first: Activity = serde_json::from_value(
            wait_payment_state(
                &client,
                fixture.api_url(),
                &first.location,
                "confirmed",
                Instant::now() + SEND_TIMEOUT,
            )
            .await?,
        )?;
        let second: Activity = serde_json::from_value(
            wait_payment_state(
                &client,
                fixture.api_url(),
                &second.location,
                "confirmed",
                Instant::now() + SEND_TIMEOUT,
            )
            .await?,
        )?;
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
            .map(|account| account.ironwood_zatoshi)
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

pub(super) async fn external_address_send_progresses_without_caller_replay() -> Result<()> {
    super::recovery::queued_address_send_progresses_without_replay().await?;
    let mut reporter = super::RecoveryFailureReporter::new();
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    let scenario = async {
        fixture.start().await?;
        reporter.phase(super::RecoveryPhase::Recovery);
        let client = Client::new();
        let address = super::faucet::external_faucet_test_address();
        let key = "external-unprepared-status-only";
        fixture.proxy().set_startup_unavailable(true)?;
        let admission = admit_payment(&client, fixture.api_url(), "/api/v1/send", &json!({"from_account":1,"to_address":address,"source_pool":"ironwood","destination_pool":"transparent","amount_zatoshi":1_000_000,"idempotency_key":key,"memo":null})).await?;
        anyhow::ensure!(admission.operation["status"] == "preparing" && admission.operation["txid"] == "", "outage admission did not retain an empty-txid intent");
        let path = admission.location;
        let pending: Vec<Activity> = request_json(&client, fixture.api_url(), &path, None, API_READ_TIMEOUT).await?;
        anyhow::ensure!(pending.len() == 1, "accepted external claim is not unique");
        let original = &pending[0];
        anyhow::ensure!(original.status == "preparing" && original.txid.is_empty() && original.to_account.is_none() && original.to_address.as_deref() == Some(address.as_str()), "outage did not retain the original empty-txid external intent");
        anyhow::ensure!(super::assertions::prepared_payment(fixture.data_dir(), &original.id)?.is_none(), "pre-prepare outage journaled signed bytes");
        fixture.interrupt_server().await?;
        fixture.proxy().set_startup_unavailable(false)?;
        fixture.restart_server().await?;
        let deadline = RegtestStack::recovery_deadline();
        let confirmed = loop {
            let stored: Vec<Activity> = fixture.recovery_read(deadline, &path).await?;
            anyhow::ensure!(stored.len() == 1 && stored[0].id == original.id, "background preparation replaced or duplicated the original claim");
            let stored = stored.into_iter().next().unwrap();
            if stored.status == "confirmed" { break stored; }
            anyhow::ensure!(stored.status != "failed", "external progression failed: {:?}", stored.last_error);
            anyhow::ensure!(std::time::Instant::now() < deadline, "accepted external intent did not prepare and confirm without caller replay");
            tokio::time::sleep(super::RECOVERY_POLL_INTERVAL).await;
        };
        anyhow::ensure!(confirmed.from_account == Some(1) && confirmed.to_account.is_none() && confirmed.to_address.as_deref() == Some(address.as_str()) && confirmed.source_pool == "ironwood" && confirmed.destination_pool == "transparent" && confirmed.amount_zatoshi == 1_000_000, "background preparation changed persisted request fields");
        let tx: serde_json::Value = rpc(&client, fixture.node_url(), "getrawtransaction", json!([confirmed.txid,1])).await?;
        anyhow::ensure!(tx["confirmations"].as_u64().is_some_and(|n| n > 0) && tx["blockhash"].as_str() == confirmed.block_hash.as_deref(), "external send lacks original transaction inclusion");
        let block: serde_json::Value = rpc(&client, fixture.node_url(), "getblockheader", json!([tx["blockhash"]])).await?;
        let canonical: String = rpc(&client, fixture.node_url(), "getblockhash", json!([block["height"]])).await?;
        anyhow::ensure!(tx["blockhash"].as_str() == Some(canonical.as_str()), "external send is not on the canonical chain");
        anyhow::ensure!(tx["vout"].as_array().is_some_and(|outputs| outputs.iter().any(|output| output["valueZat"] == 1_000_000 && (output["scriptPubKey"]["address"] == address || output["scriptPubKey"]["addresses"].as_array().is_some_and(|addresses| addresses.iter().any(|value| value == &address))))), "external transaction omitted the exact transparent destination output");
        let balance: serde_json::Value = rpc(&client, fixture.node_url(), "getaddressbalance", json!([{"addresses":[address]}])).await?;
        anyhow::ensure!(balance["balance"] == 1_000_000, "external address received more or less than one payout");
        let journal = super::assertions::prepared_payment(fixture.data_dir(), &original.id)?.ok_or_else(|| anyhow::anyhow!("background preparation omitted its wallet journal"))?;
        anyhow::ensure!(journal.txid == confirmed.txid, "wallet journal txid differs from the confirmed operation");
        let db = super::Connection::open_with_flags(fixture.data_dir().join("ths.db"), super::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let saved: (Vec<u8>,u64,Option<String>) = db.query_row("SELECT p.raw_transaction,p.expiry_height,i.memo FROM prepared_payments p JOIN idempotency i ON i.activity_id=p.activity_id WHERE i.key=?1", [key], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
        anyhow::ensure!(saved.0 == journal.raw_transaction && saved.1 == journal.expiry_height && saved.2.is_none(), "server and wallet journals disagree on bytes, expiry, or memo");
        let count: u64 = db.query_row("SELECT COUNT(*) FROM idempotency WHERE key=?1", [key], |row| row.get(0))?;
        anyhow::ensure!(count == 1, "autonomous preparation duplicated the key");
        let wallet = super::Connection::open_with_flags(fixture.data_dir().join("wallet.db"), super::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let reserved: u64 = wallet.query_row("SELECT COUNT(*) FROM ironwood_received_note_spends s JOIN transactions t ON t.id_tx=s.transaction_id WHERE t.raw=?1", [&journal.raw_transaction], |row| row.get(0))?;
        anyhow::ensure!(reserved > 0, "background preparation omitted original input reservations");
        fixture.assert_running().await?;
        Ok(())
    }.await;
    let cleanup = fixture.shutdown().await;
    if scenario.is_err() || cleanup.is_err() {
        reporter.emit(
            Some(&fixture),
            if scenario.is_ok() {
                super::FailureRoute::Cleanup
            } else {
                super::FailureRoute::Error
            },
        );
    }
    preserve_scenario_failure(scenario, cleanup)
}

pub(super) async fn disconnected_payment_preparation_keeps_the_original_transaction() -> Result<()>
{
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    let mut broadcast_proxy = None;
    let scenario = async {
        fixture.start().await?;
        broadcast_proxy = Some(QueuedBroadcastProxy::start(fixture.lightwalletd_port()?).await?);
        let proxy = broadcast_proxy.as_ref().unwrap();
        fixture.restart_with_lightwalletd(proxy.port()).await?;
        fixture.proxy().reject_payment_mining(true)?;
        proxy.pause_subtree_roots();
        let client = Client::new();
        let key = "disconnected-real-preparation";
        let body = json!({
            "from_account": 1, "to_account": 2,
            "source_pool": "ironwood", "destination_pool": "ironwood",
            "amount_zatoshi": 1_000_000, "idempotency_key": key,
        });
        let lost = lose_payment_response(&client, fixture.api_url(), "/api/v1/send", &body).await?;
        proxy.wait_for_paused_subtree_roots().await?;
        anyhow::ensure!(
            lost["status"] == "preparing" && lost["txid"] == "",
            "disconnected POST did not leave a durable claim"
        );
        // Keep the real wallet call paused beyond the ten-second dispatch budget.
        tokio::time::sleep(Duration::from_secs(11)).await;
        let duplicate = admit_payment(&client, fixture.api_url(), "/api/v1/send", &body).await?;
        anyhow::ensure!(
            duplicate.operation["id"] == lost["id"] && duplicate.operation["txid"] == "",
            "deadline or duplicate admission replaced the active preparation"
        );
        let mut conflict = body.clone();
        conflict["amount_zatoshi"] = json!(2_000_000);
        let rejected = client
            .post(format!("{}/api/v1/send", fixture.api_url()))
            .json(&conflict)
            .send()
            .await?;
        anyhow::ensure!(
            rejected.status() == reqwest::StatusCode::CONFLICT,
            "conflicting duplicate was accepted"
        );
        proxy.resume_subtree_roots();
        let location = format!("/api/v1/activity?idempotency_key={key}");
        let pending = wait_payment_state(
            &client,
            fixture.api_url(),
            &location,
            "broadcast",
            Instant::now() + SEND_TIMEOUT,
        )
        .await?;
        let original = pending_payment_snapshot(&fixture, false, key)?;
        anyhow::ensure!(
            pending["id"] == lost["id"] && pending["txid"] == original.txid,
            "wallet/server journals changed the disconnected payment identity"
        );
        fixture.restart_server().await?;
        anyhow::ensure!(
            pending_payment_snapshot(&fixture, false, key)? == original,
            "restart changed the signed original"
        );
        fixture.proxy().reject_payment_mining(false)?;
        let confirmed = wait_payment_state(
            &client,
            fixture.api_url(),
            &location,
            "confirmed",
            Instant::now() + SEND_TIMEOUT,
        )
        .await?;
        anyhow::ensure!(
            confirmed["id"] == lost["id"] && confirmed["txid"] == original.txid,
            "background recovery did not confirm the original"
        );
        fixture.assert_running().await
    }
    .await;
    if let Some(proxy) = broadcast_proxy.as_ref() {
        proxy.resume_subtree_roots();
    }
    let cleanup = fixture.shutdown().await;
    let proxy_cleanup = if let Some(proxy) = broadcast_proxy.as_mut() {
        proxy.shutdown().await
    } else {
        Ok(())
    };
    preserve_scenario_failure(scenario, preserve_scenario_failure(cleanup, proxy_cleanup))
}

pub(super) async fn expired_unseen_send_replaces_once_under_the_original_key() -> Result<()> {
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    let mut broadcast_proxy = None;
    let scenario = async {
        fixture.start().await?;
        broadcast_proxy = Some(QueuedBroadcastProxy::start(fixture.lightwalletd_port()?).await?);
        let proxy = broadcast_proxy.as_ref().unwrap();
        proxy.withhold_broadcasts(true);
        fixture.restart_with_lightwalletd(proxy.port()).await?;
        fixture.proxy().reject_payment_mining(true)?;
        let client = Client::new();
        let key = "expired-unseen-send";
        let body = json!({
            "from_account": 1, "to_account": 2,
            "source_pool": "ironwood", "destination_pool": "ironwood",
            "amount_zatoshi": 1_000_000, "idempotency_key": key,
        });
        let admission = admit_payment(&client, fixture.api_url(), "/api/v1/send", &body).await?;
        let _ = wait_payment_state(
            &client,
            fixture.api_url(),
            &admission.location,
            "broadcast",
            Instant::now() + SEND_TIMEOUT,
        )
        .await?;
        let original = pending_payment_snapshot(&fixture, false, key)?;
        let chain: ChainInfo =
            rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        let expiry_delta =
            u64::from(zcash_primitives::transaction::builder::NU7_DEFAULT_TX_EXPIRY_DELTA);
        let signed = zcash_primitives::transaction::Transaction::read(
            original.raw.as_slice(),
            zcash_protocol::consensus::BranchId::Nu7,
        )?;
        let signed_expiry = u64::from(u32::from(signed.expiry_height()));
        anyhow::ensure!(
            signed_expiry == original.expiry_height,
            "journal expiry differs from signed transaction"
        );
        let blocks = signed_expiry
            .checked_sub(chain.blocks)
            .context("signed transaction expired before expiry scenario advanced the chain")?
            + 1;
        anyhow::ensure!(
            blocks > 0 && blocks <= expiry_delta + 2,
            "unexpected expiry distance: {blocks}"
        );
        let _: serde_json::Value = rpc_with_timeout(
            &client,
            fixture.node_url(),
            "generate",
            json!([blocks]),
            Duration::from_secs(120),
        )
        .await?;
        let mempool: Vec<String> =
            rpc(&client, fixture.node_url(), "getrawmempool", json!([])).await?;
        anyhow::ensure!(
            !mempool.contains(&original.txid),
            "original transaction reached the chain node"
        );
        proxy.withhold_broadcasts(false);
        fixture.proxy().reject_payment_mining(false)?;
        let confirmed = wait_payment_state(
            &client,
            fixture.api_url(),
            &admission.location,
            "confirmed",
            Instant::now() + SEND_TIMEOUT,
        )
        .await?;
        anyhow::ensure!(
            confirmed["id"] == original.id && confirmed["txid"] != original.txid,
            "expired original was not safely replaced under its key"
        );
        let replacement = confirmed["txid"]
            .as_str()
            .context("replacement has no txid")?;
        let evidence: serde_json::Value = rpc(
            &client,
            fixture.node_url(),
            "getrawtransaction",
            json!([replacement, 1]),
        )
        .await?;
        anyhow::ensure!(
            evidence["confirmations"].as_u64().is_some_and(|n| n > 0)
                && evidence["blockhash"] == confirmed["block_hash"],
            "replacement lacks canonical inclusion"
        );
        let block: serde_json::Value = rpc(
            &client,
            fixture.node_url(),
            "getblockheader",
            json!([evidence["blockhash"]]),
        )
        .await?;
        let canonical: String = rpc(
            &client,
            fixture.node_url(),
            "getblockhash",
            json!([block["height"]]),
        )
        .await?;
        anyhow::ensure!(
            evidence["blockhash"] == canonical,
            "replacement is not on the canonical chain"
        );
        let accounts: Vec<AccountBalance> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/accounts",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        anyhow::ensure!(
            accounts
                .iter()
                .any(|a| a.id == 2 && a.ironwood_zatoshi == 1_000_000),
            "replacement did not produce exactly one SDK payout"
        );
        let replay = admit_payment(&client, fixture.api_url(), "/api/v1/send", &body).await?;
        anyhow::ensure!(
            replay.operation == confirmed,
            "terminal replay changed the replacement"
        );
        fixture.assert_running().await
    }
    .await;
    let cleanup = fixture.shutdown().await;
    let proxy_cleanup = if let Some(proxy) = broadcast_proxy.as_mut() {
        proxy.shutdown().await
    } else {
        Ok(())
    };
    preserve_scenario_failure(scenario, preserve_scenario_failure(cleanup, proxy_cleanup))
}
