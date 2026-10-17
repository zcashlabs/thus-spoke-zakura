use super::*;
use super::{
    API_READ_TIMEOUT, AccountBalance, Activity, Address, BlockHeight, ChainInfo, Client,
    Connection, Context, Duration, GenerateCounts, HeightCheckpoint, Instant, LiveScenario,
    LocalNetwork, OpenFlags, PREPARE_RESTART_IDEMPOTENCY_KEY, PREPARE_ROLLBACK_IDEMPOTENCY_KEY,
    PathBuf, PreparedPaymentRecord, QueuedBroadcastProxy, RECOVERY_IDEMPOTENCY_KEY,
    RECOVERY_POLL_INTERVAL, RecoveryFailureReporter, RecoveryPhase, RegtestStack, Result,
    SEND_TIMEOUT, Status, TransactionEvidence, TransparentAddress, assert_persisted_send_response,
    assert_read_only_persistence, assert_requested_payment_fields, constructed_transactions, json,
    lose_payment_response, one_matching_activity, params, pending_payment_snapshot,
    prepared_payment, prepared_payment_journal, preserve_scenario_failure, request_json, rpc,
    run_live_scenario, support,
};

pub(super) async fn prepare_rolls_back_constructed_transaction_when_journal_write_fails()
-> Result<()> {
    let server = PathBuf::from(env!("CARGO_BIN_EXE_ths-server"));
    let mut fixture = RegtestStack::new(server)?;
    let scenario = async {
        fixture.start().await?;
        let transactions_before = constructed_transactions(fixture.data_dir())?;
        let journal_before = prepared_payment_journal(fixture.data_dir())?;
        let wallet_database = Connection::open(fixture.data_dir().join("wallet.db"))?;
        wallet_database.execute_batch(
            "CREATE TRIGGER fail_prepared_payment_journal
             BEFORE INSERT ON ext_tsz_prepared_payments
             BEGIN SELECT RAISE(ABORT, 'injected prepared-payment journal failure'); END;",
        )?;

        let client = Client::new();
        let admission = admit_payment(
            &client,
            fixture.api_url(),
            "/api/v1/send",
            &json!({
                "from_account": 1, "to_account": 2,
                "source_pool": "ironwood", "destination_pool": "ironwood",
                "amount_zatoshi": 1_000_000,
                "idempotency_key": PREPARE_ROLLBACK_IDEMPOTENCY_KEY,
            }),
        )
        .await?;
        let deadline = Instant::now() + SEND_TIMEOUT;
        loop {
            let status: serde_json::Value = request_json(
                &client,
                fixture.api_url(),
                &admission.location,
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            let pending = &status[0];
            anyhow::ensure!(
                pending["id"] == admission.operation["id"]
                    && pending["status"] == "preparing"
                    && pending["txid"] == "",
                "failed journal preparation advanced the durable intent"
            );
            if pending["last_error"]
                .as_str()
                .is_some_and(|error| error.contains("injected prepared-payment journal failure"))
            {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "journal failure was not recorded in status"
            );
            tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
        }
        fixture.interrupt_server().await?;
        fixture.restart_server().await?;
        anyhow::ensure!(
            constructed_transactions(fixture.data_dir())? == transactions_before,
            "constructed transaction survived the failed journal write"
        );
        anyhow::ensure!(
            prepared_payment_journal(fixture.data_dir())? == journal_before,
            "prepared-payment journal changed despite transaction rollback"
        );
        fixture.interrupt_server().await?;
        wallet_database.execute_batch("DROP TRIGGER fail_prepared_payment_journal")?;
        drop(wallet_database);
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

pub(super) async fn broadcast_recovers_after_auto_mine_failure() -> Result<()> {
    run_live_scenario(LiveScenario::BroadcastRecovery).await
}

pub(super) async fn queued_retry_survives_restart(external: bool) -> Result<()> {
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    let mut broadcast_proxy = None;
    let scenario = async {
        fixture.start().await?;
        broadcast_proxy = Some(QueuedBroadcastProxy::start(fixture.lightwalletd_port()?).await?);
        let proxy = broadcast_proxy.as_ref().unwrap();
        fixture.restart_with_lightwalletd(proxy.port()).await?;
        let client = Client::new();
        let key = "queued-lost-response-restart";
        let one = Some(BlockHeight::from_u32(1));
        let network = LocalNetwork { overwinter: one, sapling: one, blossom: one, heartwood: one, canopy: one, nu5: one, nu6: one, nu6_1: one, nu6_2: one, nu6_3: one, nu7: None };
        let address = Address::Transparent(TransparentAddress::PublicKeyHash([43; 20])).encode(&network);
        let (path, body) = if external {
            ("/api/v1/faucet/address", json!({"address": address, "amount_zatoshi": 1_000_000, "idempotency_key": key}))
        } else {
            ("/api/v1/send", json!({"from_account": 1, "to_account": 2, "source_pool": "ironwood", "destination_pool": "ironwood", "amount_zatoshi": 1_000_000, "idempotency_key": key}))
        };
        fixture.proxy().reject_payment_mining(true)?;
        let lost = lose_payment_response(&client, fixture.api_url(), path, &body).await?;
        anyhow::ensure!(lost["status"] == "preparing" && lost["txid"] == "" && lost["block_hash"].is_null(), "lost admission response was not a durable preparing intent");
        let location = if external {
            format!("/api/v1/faucet/address?idempotency_key={key}")
        } else {
            format!("/api/v1/activity?idempotency_key={key}")
        };
        let pending = wait_payment_state(&client, fixture.api_url(), &location, "broadcast", Instant::now() + SEND_TIMEOUT).await?;
        let original = pending_payment_snapshot(&fixture, external, key)?;
        anyhow::ensure!(pending["txid"].as_str() == Some(original.txid.as_str()), "pending status and journals disagree");
        if !external {
            anyhow::ensure!(lost["id"].as_str() == Some(original.id.as_str()), "background progression replaced admitted activity identity");
        }
        let mempool: Vec<String> = rpc(&client, fixture.node_url(), "getrawmempool", json!([])).await?;
        anyhow::ensure!(mempool.contains(&original.txid), "original broadcast did not reach the real node");

        fixture.proxy().hide_transaction(Some(original.txid.clone()))?;
        proxy.queue_next_broadcast();
        let retry = admit_payment(&client, fixture.api_url(), path, &body).await?;
        anyhow::ensure!(retry.operation["txid"].as_str() == Some(original.txid.as_str()) && retry.operation["status"] == "broadcast", "same-key admission changed the retained pending payment");
        let deadline = Instant::now() + SEND_TIMEOUT;
        while proxy.broadcast_counts().1 == 0 {
            anyhow::ensure!(Instant::now() < deadline, "background recovery did not exercise the queued response");
            tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
        }
        proxy.assert_identical_retry()?;
        anyhow::ensure!(fixture.proxy().hidden_lookups()? > 0, "queued retry did not exercise missing transaction lookup");
        anyhow::ensure!(pending_payment_snapshot(&fixture, external, key)? == original, "queued retry changed journals or reserved inputs");

        fixture.restart_server().await?;
        anyhow::ensure!(pending_payment_snapshot(&fixture, external, key)? == original, "reopening stores changed the pending original payment");
        fixture.proxy().hide_transaction(None)?;
        let _: serde_json::Value = rpc(&client, fixture.node_url(), "generate", json!([1])).await?;
        let confirmed = wait_payment_state(&client, fixture.api_url(), &location, "confirmed", Instant::now() + SEND_TIMEOUT).await?;
        anyhow::ensure!(confirmed["status"] == "confirmed" && confirmed["txid"].as_str() == Some(original.txid.as_str()), "background recovery did not confirm the original transaction");
        let evidence: serde_json::Value = rpc(&client, fixture.node_url(), "getrawtransaction", json!([original.txid, 1])).await?;
        anyhow::ensure!(evidence["confirmations"].as_u64().is_some_and(|n| n > 0) && evidence["blockhash"].is_string() && confirmed["block_hash"] == evidence["blockhash"], "confirmation lacks canonical transaction evidence");
        let block: serde_json::Value = rpc(&client, fixture.node_url(), "getblockheader", json!([evidence["blockhash"]])).await?;
        let canonical: String = rpc(&client, fixture.node_url(), "getblockhash", json!([block["height"]])).await?;
        anyhow::ensure!(evidence["blockhash"].as_str() == Some(canonical.as_str()), "confirmed transaction is not on the canonical chain");
        let before: ChainInfo = rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        let broadcasts_before = proxy.broadcast_counts();
        let replay = admit_payment(&client, fixture.api_url(), path, &body).await?.operation;
        anyhow::ensure!(replay == confirmed, "confirmed replay changed the payment");
        anyhow::ensure!(proxy.broadcast_counts() == broadcasts_before, "confirmed replay broadcast another transaction");
        proxy.assert_identical_retry()?;
        let after: ChainInfo = rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        anyhow::ensure!(before.blocks == after.blocks && before.bestblockhash == after.bestblockhash, "confirmed replay mined another payment");
        let mempool: Vec<String> = rpc(&client, fixture.node_url(), "getrawmempool", json!([])).await?;
        anyhow::ensure!(mempool.is_empty(), "confirmed replay broadcast another transaction");
        if external {
            let balance: serde_json::Value = rpc(&client, fixture.node_url(), "getaddressbalance", json!([{"addresses": [address]}])).await?;
            anyhow::ensure!(balance["balance"] == 1_000_000, "external destination did not receive exactly one payout");
        } else {
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                let accounts: Vec<AccountBalance> = request_json(&client, fixture.api_url(), "/api/v1/accounts", None, API_READ_TIMEOUT).await?;
                if accounts.iter().any(|a| a.id == 2 && a.ironwood_zatoshi == 1_000_000) { break; }
                anyhow::ensure!(Instant::now() < deadline, "Send destination did not receive exactly one payout");
                tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
            }
        }
        let db = Connection::open_with_flags(fixture.data_dir().join("ths.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let count: u64 = db.query_row(if external { "SELECT COUNT(*) FROM address_faucets WHERE key=?1" } else { "SELECT COUNT(*) FROM idempotency WHERE key=?1" }, [key], |row| row.get(0))?;
        anyhow::ensure!(count == 1, "retry created duplicate operations");
        fixture.assert_running().await?;
        Ok(())
    }.await;
    let cleanup = fixture.shutdown().await;
    let proxy_cleanup = if let Some(proxy) = broadcast_proxy.as_mut() {
        proxy.shutdown().await
    } else {
        Ok(())
    };
    preserve_scenario_failure(scenario, preserve_scenario_failure(cleanup, proxy_cleanup))
}

pub(super) async fn queued_address_send_progresses_without_replay() -> Result<()> {
    queued_payment_survives_restart(false, true).await
}

async fn queued_payment_survives_restart(external: bool, external_send: bool) -> Result<()> {
    let mut reporter = super::RecoveryFailureReporter::new();
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    let mut broadcast_proxy = None;
    let scenario = async {
        fixture.start().await?;
        reporter.phase(super::RecoveryPhase::Recovery);
        broadcast_proxy = Some(QueuedBroadcastProxy::start(fixture.lightwalletd_port()?).await?);
        let proxy = broadcast_proxy.as_ref().unwrap();
        fixture.restart_with_lightwalletd(proxy.port()).await?;
        let client = Client::new();
        let key = "queued-lost-response-restart";
        let one = Some(BlockHeight::from_u32(1));
        let network = LocalNetwork { overwinter: one, sapling: one, blossom: one, heartwood: one, canopy: one, nu5: one, nu6: one, nu6_1: one, nu6_2: one, nu6_3: one, nu7: None };
        let address = Address::Transparent(TransparentAddress::PublicKeyHash([43; 20])).encode(&network);
        let (path, body) = if external_send {
            ("/api/v1/send", json!({"from_account":1,"to_address":address,"source_pool":"ironwood","destination_pool":"transparent","amount_zatoshi":1_000_000,"idempotency_key":key,"memo":null}))
        } else if external {
            ("/api/v1/faucet/address", json!({"address": address, "amount_zatoshi": 1_000_000, "idempotency_key": key}))
        } else {
            ("/api/v1/send", json!({"from_account": 1, "to_account": 2, "source_pool": "ironwood", "destination_pool": "ironwood", "amount_zatoshi": 1_000_000, "idempotency_key": key}))
        };
        fixture.proxy().reject_payment_mining(true)?;
        let lost = lose_payment_response(&client, fixture.api_url(), path, &body).await?;
        anyhow::ensure!(lost["status"] == "preparing" && lost["txid"] == "" && lost["block_hash"].is_null(), "lost response did not retain the admitted intent");
        let location = format!("{}?idempotency_key={key}", if external { "/api/v1/faucet/address" } else { "/api/v1/activity" });
        let pending = wait_payment_state(&client, fixture.api_url(), &location, "broadcast", Instant::now() + SEND_TIMEOUT).await?;
        let original = pending_payment_snapshot(&fixture, external, key)?;
        anyhow::ensure!(pending["txid"].as_str() == Some(original.txid.as_str()), "pending status and journals disagree");
        if !external { anyhow::ensure!(lost["id"].as_str() == Some(original.id.as_str()), "background progression replaced admitted identity"); }
        let mempool: Vec<String> = rpc(&client, fixture.node_url(), "getrawmempool", json!([])).await?;
        anyhow::ensure!(mempool.contains(&original.txid), "original broadcast did not reach the real node");

        fixture.proxy().hide_transaction(Some(original.txid.clone()))?;
        fixture.proxy().reject_payment_mining(true)?;
        proxy.queue_next_broadcast();
        if !external_send {
            let retry = admit_payment(&client, fixture.api_url(), path, &body).await?;
            anyhow::ensure!(retry.operation["txid"].as_str() == Some(original.txid.as_str()) && retry.operation["status"] == "broadcast", "replay changed retained payment identity");
        }
        let deadline = Instant::now() + SEND_TIMEOUT;
        while proxy.broadcast_counts().1 == 0 {
            anyhow::ensure!(Instant::now() < deadline, "background recovery did not exercise the queued response");
            tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
        }
        proxy.assert_identical_retry()?;
        anyhow::ensure!(fixture.proxy().hidden_lookups()? > 0, "queued retry did not exercise missing transaction lookup");
        anyhow::ensure!(pending_payment_snapshot(&fixture, external, key)? == original, "queued retry changed journals or reserved inputs");

        fixture.restart_server().await?;
        anyhow::ensure!(pending_payment_snapshot(&fixture, external, key)? == original, "reopening stores changed the pending original payment");
        fixture.proxy().hide_transaction(None)?;
        let _: serde_json::Value = rpc(&client, fixture.node_url(), "generate", json!([1])).await?;
        // The preceding phases deliberately replayed queued bytes. After restart and restore,
        // the external faucet must finish with status reads alone before terminal replay checks.
        let confirmed: serde_json::Value = if external || external_send {
            let deadline = RegtestStack::recovery_deadline();
            loop {
                let stored: serde_json::Value = fixture.recovery_read(deadline, &format!("{}?idempotency_key={key}", if external { "/api/v1/faucet/address" } else { "/api/v1/activity" })).await?;
                let stored = if external_send { stored[0].clone() } else { stored };
                if stored["status"] == "confirmed" { break stored; }
                anyhow::ensure!(Instant::now() < deadline, "external faucet did not progress under status-only observation");
                tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
            }
        } else {
            request_json(&client, fixture.api_url(), path, Some(&body), SEND_TIMEOUT).await?
        };
        anyhow::ensure!(confirmed["status"] == "confirmed" && confirmed["txid"].as_str() == Some(original.txid.as_str()), "retry did not confirm the original transaction");
        let evidence: serde_json::Value = rpc(&client, fixture.node_url(), "getrawtransaction", json!([original.txid, 1])).await?;
        anyhow::ensure!(evidence["confirmations"].as_u64().is_some_and(|n| n > 0) && evidence["blockhash"].is_string() && confirmed["block_hash"] == evidence["blockhash"], "confirmation lacks canonical transaction evidence");
        let block: serde_json::Value = rpc(&client, fixture.node_url(), "getblockheader", json!([evidence["blockhash"]])).await?;
        let canonical: String = rpc(&client, fixture.node_url(), "getblockhash", json!([block["height"]])).await?;
        anyhow::ensure!(evidence["blockhash"].as_str() == Some(canonical.as_str()), "confirmed transaction is not on the canonical chain");
        if external_send {
            proxy.assert_identical_broadcasts()?;
            anyhow::ensure!(confirmed["to_account"].is_null() && confirmed["to_address"] == address, "external send lost recipient identity");
            let journal = prepared_payment(fixture.data_dir(), &original.id)?.context("original wallet journal disappeared")?;
            anyhow::ensure!(journal.txid == original.txid && journal.raw_transaction == original.raw && journal.expiry_height == original.expiry_height, "external confirmation changed the original wallet preparation");
            let db = Connection::open_with_flags(fixture.data_dir().join("ths.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let saved: (Vec<u8>, u64) = db.query_row("SELECT raw_transaction,expiry_height FROM prepared_payments WHERE activity_id=?1", [&original.id], |row| Ok((row.get(0)?,row.get(1)?)))?;
            anyhow::ensure!(saved == (original.raw.clone(), original.expiry_height), "external confirmation changed server signed bytes or expiry");
            let memo: Option<String> = db.query_row("SELECT memo FROM idempotency WHERE key=?1", [key], |row| row.get(0))?;
            anyhow::ensure!(memo.is_none(), "transparent external send acquired a memo");
        } else {
        let before: ChainInfo = rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        let replay: serde_json::Value = request_json(&client, fixture.api_url(), path, Some(&body), SEND_TIMEOUT).await?;
        anyhow::ensure!(replay == confirmed, "confirmed replay changed the payment");
        proxy.assert_identical_retry()?;
        let after: ChainInfo = rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        anyhow::ensure!(before.blocks == after.blocks && before.bestblockhash == after.bestblockhash, "confirmed replay mined another payment");

        }
        let mempool: Vec<String> = rpc(&client, fixture.node_url(), "getrawmempool", json!([])).await?;
        anyhow::ensure!(mempool.is_empty(), "confirmed replay broadcast another transaction");
        if external || external_send {
            let balance: serde_json::Value = rpc(&client, fixture.node_url(), "getaddressbalance", json!([{"addresses": [address]}])).await?;
            anyhow::ensure!(balance["balance"] == 1_000_000, "external destination did not receive exactly one payout");
        } else {
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                let accounts: Vec<AccountBalance> = request_json(&client, fixture.api_url(), "/api/v1/accounts", None, API_READ_TIMEOUT).await?;
                if accounts.iter().any(|a| a.id == 2 && a.ironwood_zatoshi == 1_000_000) { break; }
                anyhow::ensure!(Instant::now() < deadline, "Send destination did not receive exactly one payout");
                tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
            }
        }
        let db = Connection::open_with_flags(fixture.data_dir().join("ths.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let count: u64 = db.query_row(if external { "SELECT COUNT(*) FROM address_faucets WHERE key=?1" } else { "SELECT COUNT(*) FROM idempotency WHERE key=?1" }, [key], |row| row.get(0))?;
        anyhow::ensure!(count == 1, "retry created duplicate operations");
        fixture.assert_running().await?;
        Ok(())
    }.await;
    let cleanup = fixture.shutdown().await;
    let proxy_cleanup = if let Some(proxy) = broadcast_proxy.as_mut() {
        proxy.shutdown().await
    } else {
        Ok(())
    };
    let cleanup = preserve_scenario_failure(cleanup, proxy_cleanup);
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

fn seed_preparing_payment(path: &std::path::Path, idempotency_key: &str) -> Result<String> {
    let activity_id = uuid::Uuid::new_v4().to_string();
    let mut database = Connection::open(path.join("ths.db"))?;
    let transaction = database.transaction()?;
    transaction.execute(
        "INSERT INTO activity(
           id,kind,from_account,to_account,source_pool,destination_pool,
           amount_zatoshi,txid,status
         ) VALUES(?1,'send',1,2,'ironwood','ironwood',1000000,'','preparing')",
        [&activity_id],
    )?;
    transaction.execute(
        "INSERT INTO idempotency(key,activity_id,memo) VALUES(?1,?2,NULL)",
        params![idempotency_key, activity_id],
    )?;
    transaction.commit()?;
    Ok(activity_id)
}

async fn wait_for_prepared_payment(
    path: &std::path::Path,
    activity_id: &str,
) -> Result<PreparedPaymentRecord> {
    let deadline = Instant::now() + SEND_TIMEOUT;
    loop {
        if let Some(prepared) = prepared_payment(path, activity_id)? {
            return Ok(prepared);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "wallet preparation did not commit before the deadline"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub(super) async fn exercise_recovery(
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
    fixture.proxy().reject_payment_mining(true)?;
    let admission = admit_payment(
        &client,
        fixture.api_url(),
        "/api/v1/send",
        &json!({
            "from_account": 1,
            "to_account": 2,
            "source_pool": "ironwood",
            "destination_pool": "ironwood",
            "amount_zatoshi": 1000000,
            "idempotency_key": RECOVERY_IDEMPOTENCY_KEY,
        }),
    )
    .await?;
    let fault_deadline = Instant::now() + SEND_TIMEOUT;
    let (sent, proxy_after_auto_mine): (Activity, GenerateCounts) = loop {
        let pending = wait_payment_state(
            &client,
            fixture.api_url(),
            &admission.location,
            "broadcast",
            fault_deadline,
        )
        .await?;
        let counts = fixture.proxy().counts();
        if counts.rejected > proxy_before.rejected
            && pending["last_error"]
                .as_str()
                .is_some_and(|error| error.contains("injected auto-mine failure"))
        {
            break (serde_json::from_value(pending)?, counts);
        }
        wait_for_recovery_poll(fault_deadline).await?;
    };
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

    anyhow::ensure!(
        proxy_after_auto_mine.rejected > proxy_before.rejected
            && proxy_after_auto_mine.forwarded == proxy_before.forwarded,
        "the injected auto-mine fault was not observed or forwarded a mining request"
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
    // Background reconciliation may clear the transient mining error while the
    // transaction remains broadcast. All other activity fields must stay fixed.
    anyhow::ensure!(
        persisted_broadcast.last_error == sent.last_error
            || persisted_broadcast.last_error.is_none(),
        "broadcast activity acquired an unexpected error"
    );
    let mut expected_broadcast = sent.clone();
    expected_broadcast.last_error = persisted_broadcast.last_error.clone();
    assert_persisted_send_response(&expected_broadcast, &persisted_broadcast)?;

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
    expected_confirmed.last_error = None;
    anyhow::ensure!(
        confirmed == expected_confirmed,
        "activity changed beyond its confirmation status, inclusion block hash, and cleared mining error: expected {expected_confirmed:?}, got {confirmed:?}"
    );
    assert_requested_payment_fields(&confirmed)?;
    anyhow::ensure!(
        confirmed.status == "confirmed",
        "recovered activity was not confirmed"
    );
    // Keep server mining faulted until direct-node inclusion is reconciled.
    // Autonomous retries may be rejected, but none may advance the chain.
    let recovery_counts = fixture.proxy().counts();
    anyhow::ensure!(
        recovery_counts.rejected >= proxy_after_auto_mine.rejected
            && recovery_counts.forwarded == proxy_after_auto_mine.forwarded,
        "recovery forwarded a mining request despite the fault: {recovery_counts:?}"
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
        fixture.data_dir().join("ths.db"),
        &persisted_broadcast,
        &expected_block_hash,
    )?;
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

pub(super) async fn startup_outage_serves_stored_status_and_recovers() -> Result<()> {
    let mut reporter = super::RecoveryFailureReporter::new();
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    let scenario = async {
        fixture.start().await?;
        reporter.phase(super::RecoveryPhase::Recovery);
        let real_lightwalletd = fixture.lightwalletd_port()?;
        let client = Client::new();
        let address = super::faucet::external_faucet_test_address();
        let external_key = "outage-stored-external";
        let external: serde_json::Value = request_json(&client, fixture.api_url(), "/api/v1/faucet/address", Some(&json!({"address":address,"amount_zatoshi":1_000_000,"idempotency_key":external_key})), SEND_TIMEOUT).await?;
        anyhow::ensure!(external["status"] == "confirmed", "external fixture payment did not settle");
        let internal_path = "/api/v1/activity?idempotency_key=startup-account-1-ironwood-v1";
        let external_path = format!("/api/v1/faucet/address?idempotency_key={external_key}");
        let internal: serde_json::Value = request_json(&client, fixture.api_url(), internal_path, None, API_READ_TIMEOUT).await?;
        let balances: serde_json::Value = request_json(&client, fixture.api_url(), "/api/v1/accounts", None, API_READ_TIMEOUT).await?;
        let wallet_before = prepared_payment_journal(fixture.data_dir())?;
        let server_before = super::assertions::server_prepared_journal(fixture.data_dir())?;
        let constructed_before = constructed_transactions(fixture.data_dir())?;
        let counts = fixture.proxy().counts();
        let tip: ChainInfo = rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        fixture.interrupt_server().await?;
        fixture.proxy().set_startup_unavailable(true)?;
        fixture.restart_server_without_readiness(None).await?;
        let started = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(150);
        loop {
            match fixture.recovery_read::<serde_json::Value>(deadline, internal_path).await {
                Ok(stored) => {
                    anyhow::ensure!(stored == internal, "outage changed stored internal status");
                    let stored: serde_json::Value = fixture.recovery_read(deadline, &external_path).await?;
                    anyhow::ensure!(stored == external, "outage changed stored external status");
                    let health = client.get(format!("{}/api/v1/health",fixture.api_url())).timeout(API_READ_TIMEOUT).send().await?;
                    anyhow::ensure!(health.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE, "startup outage reported healthy");
                    let mutation = client.post(format!("{}/api/v1/send",fixture.api_url())).timeout(API_READ_TIMEOUT).json(&json!({"from_account":1,"to_account":2,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":1_000_000,"idempotency_key":"outage-must-not-claim"})).send().await?;
                    anyhow::ensure!(mutation.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE, "startup outage admitted a mutation");
                    if started.elapsed() > Duration::from_secs(122) { break; }
                }
                Err(error) if support::is_retryable_read_transport(&error) => {}
                Err(error) => return Err(error),
            }
            anyhow::ensure!(Instant::now() < deadline, "stored outage observation exceeded its deadline");
            tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
        }
        fixture.assert_running().await?;
        anyhow::ensure!(super::assertions::server_prepared_journal(fixture.data_dir())? == server_before, "outage changed server prepared bytes or expiry");
        anyhow::ensure!(prepared_payment_journal(fixture.data_dir())? == wallet_before && constructed_transactions(fixture.data_dir())? == constructed_before, "outage changed wallet journal or constructed transactions");
        let db = Connection::open_with_flags(fixture.data_dir().join("ths.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let claims: u64 = db.query_row("SELECT COUNT(*) FROM idempotency WHERE key='outage-must-not-claim'", [], |row| row.get(0))?;
        anyhow::ensure!(claims == 0, "startup mutation created a claim");
        anyhow::ensure!(fixture.proxy().counts() == counts, "startup outage broadcast or mined");
        let current: ChainInfo = rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        anyhow::ensure!(current.blocks == tip.blocks && current.bestblockhash == tip.bestblockhash, "outage moved the real chain");
        fixture.proxy().set_startup_unavailable(false)?;
        fixture.wait_until_ready().await?;
        let recovered: serde_json::Value = request_json(&client, fixture.api_url(), "/api/v1/accounts", None, API_READ_TIMEOUT).await?;
        anyhow::ensure!(recovered == balances, "recovery changed SDK balances");
        let unavailable = std::net::TcpListener::bind("127.0.0.1:0")?;
        let unavailable_port = unavailable.local_addr()?.port();
        drop(unavailable);
        fixture.restart_server_without_readiness(Some(unavailable_port)).await?;
        let deadline = RegtestStack::recovery_deadline();
        loop {
            match fixture.recovery_read::<serde_json::Value>(deadline, &external_path).await {
                Ok(stored) => { anyhow::ensure!(stored == external, "unavailable lightwalletd changed stored status"); break; }
                Err(error) if support::is_retryable_read_transport(&error) => {}
                Err(error) => return Err(error),
            }
            anyhow::ensure!(Instant::now() < deadline, "stored status required lightwalletd");
            tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
        }
        let health = client.get(format!("{}/api/v1/health",fixture.api_url())).timeout(API_READ_TIMEOUT).send().await?;
        anyhow::ensure!(health.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE, "unavailable lightwalletd reported healthy");
        fixture.restart_server_without_readiness(Some(real_lightwalletd)).await?;
        fixture.wait_until_ready().await?;
        let restored: serde_json::Value = request_json(&client, fixture.api_url(), internal_path, None, API_READ_TIMEOUT).await?;
        anyhow::ensure!(restored == internal, "dependency recovery replaced the original operation");
        anyhow::ensure!(prepared_payment_journal(fixture.data_dir())? == wallet_before, "dependency recovery replaced original signed bytes");
        anyhow::ensure!(super::assertions::server_prepared_journal(fixture.data_dir())? == server_before, "recovery changed server prepared bytes or expiry");
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

pub(super) async fn background_recovers_prepare_interrupted_before_store_recording() -> Result<()> {
    let server = PathBuf::from(env!("CARGO_BIN_EXE_ths-server"));
    let mut fixture = RegtestStack::new(server)?;
    let scenario = async {
        fixture
            .start()
            .await
            .context("starting prepare-restart fixture")?;
        let client = Client::new();
        let accounts_before: Vec<AccountBalance> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/accounts",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let balance_before = accounts_before
            .iter()
            .find(|account| account.id == 2)
            .map(|account| account.ironwood_zatoshi)
            .context("destination account is missing")?;
        let request = json!({
            "from_account": 1,
            "to_account": 2,
            "source_pool": "ironwood",
            "destination_pool": "ironwood",
            "amount_zatoshi": 1_000_000,
            "idempotency_key": PREPARE_RESTART_IDEMPOTENCY_KEY,
        });
        // Seed the durable intent so admission can finish before holding the server writer.
        let activity_id =
            seed_preparing_payment(fixture.data_dir(), PREPARE_RESTART_IDEMPOTENCY_KEY)?;
        let admission = admit_payment(&client, fixture.api_url(), "/api/v1/send", &request).await?;
        anyhow::ensure!(
            admission.operation["id"] == activity_id
                && admission.operation["status"] == "preparing",
            "replay did not return the seeded durable intent"
        );
        let application_lock = Connection::open(fixture.data_dir().join("ths.db"))?;
        // Background construction commits the wallet journal before its server write.
        application_lock.execute_batch("BEGIN IMMEDIATE")?;
        let original = wait_for_prepared_payment(fixture.data_dir(), &activity_id)
            .await
            .context("waiting for wallet preparation to commit")?;
        anyhow::ensure!(
            !original.raw_transaction.is_empty() && original.expiry_height > 0,
            "wallet journal did not retain complete prepared transaction data"
        );
        fixture
            .interrupt_server()
            .await
            .context("interrupting server after wallet preparation")?;
        application_lock.execute_batch("ROLLBACK")?;
        drop(application_lock);
        let application_database = Connection::open(fixture.data_dir().join("ths.db"))?;
        let (txid, status): (String, String) = application_database.query_row(
            "SELECT txid,status FROM activity WHERE id=?1",
            [&activity_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        anyhow::ensure!(
            txid.is_empty() && status == "preparing",
            "application database recorded preparation before interruption"
        );
        let application_prepared: u32 = application_database.query_row(
            "SELECT COUNT(*) FROM prepared_payments WHERE activity_id=?1",
            [&activity_id],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            application_prepared == 0,
            "application database retained prepared bytes before retry"
        );
        drop(application_database);

        fixture
            .restart_server()
            .await
            .context("restarting server after interrupted preparation")?;
        let recovered: Activity = serde_json::from_value(
            wait_payment_state(
                &client,
                fixture.api_url(),
                &admission.location,
                "confirmed",
                Instant::now() + SEND_TIMEOUT,
            )
            .await
            .context("waiting for interrupted payment without another POST")?,
        )?;
        anyhow::ensure!(
            recovered.id == activity_id
                && recovered.txid == original.txid
                && recovered.status == "confirmed",
            "background recovery did not confirm the original activity and transaction"
        );
        let application_database = Connection::open(fixture.data_dir().join("ths.db"))?;
        let recorded: (Vec<u8>, u64) = application_database.query_row(
            "SELECT raw_transaction,expiry_height FROM prepared_payments WHERE activity_id=?1",
            [&activity_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        anyhow::ensure!(
            recorded == (original.raw_transaction.clone(), original.expiry_height),
            "retry did not recover the original serialized bytes and expiry height"
        );

        let accounts_after: Vec<AccountBalance> = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/accounts",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let balance_after = accounts_after
            .iter()
            .find(|account| account.id == 2)
            .map(|account| account.ironwood_zatoshi)
            .context("destination account is missing after retry")?;
        anyhow::ensure!(
            balance_after.checked_sub(balance_before) == Some(1_000_000),
            "retry produced more or less than one payout"
        );
        let transaction: TransactionEvidence = rpc(
            &client,
            fixture.node_url(),
            "getrawtransaction",
            json!([original.txid, 1]),
        )
        .await?;
        anyhow::ensure!(
            transaction
                .confirmations
                .is_some_and(|confirmations| confirmations > 0),
            "recovered transaction was not confirmed"
        );
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

pub(super) async fn restart_recovers_claim_before_preparation_without_payment_post() -> Result<()> {
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    let scenario = async {
        fixture.start().await?;
        fixture.interrupt_server().await?;
        let key = "claim-before-worker-restart";
        let id = seed_preparing_payment(fixture.data_dir(), key)?;
        anyhow::ensure!(
            prepared_payment(fixture.data_dir(), &id)?.is_none(),
            "claim was already prepared before restart"
        );
        fixture.restart_server().await?;
        let client = Client::new();
        let payment: Activity = serde_json::from_value(
            wait_payment_state(
                &client,
                fixture.api_url(),
                &format!("/api/v1/activity?idempotency_key={key}"),
                "confirmed",
                Instant::now() + SEND_TIMEOUT,
            )
            .await?,
        )?;
        anyhow::ensure!(
            payment.id == id && payment.status == "confirmed",
            "reopened claim did not complete"
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
            "reopened claim did not produce exactly one SDK balance increase"
        );
        fixture.assert_running().await
    }
    .await;
    let cleanup = fixture.shutdown().await;
    scenario?;
    cleanup
}

pub(super) async fn insufficient_send_stays_failed_after_restart_and_later_funding() -> Result<()> {
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    let scenario = async {
        fixture.start().await?;
        let client = Client::new();
        let key = "insufficient-send-later-funded";
        let body = json!({
            "from_account": 1, "to_account": 2,
            "source_pool": "ironwood", "destination_pool": "ironwood",
            "amount_zatoshi": 600_000_000, "idempotency_key": key,
        });
        let admission = admit_payment(&client, fixture.api_url(), "/api/v1/send", &body).await?;
        let deadline = Instant::now() + SEND_TIMEOUT;
        let failed = loop {
            let rows: Vec<serde_json::Value> = request_json(
                &client,
                fixture.api_url(),
                &admission.location,
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            let row = rows.first().context("accepted Send disappeared")?;
            if row["status"] == "failed" {
                break row.clone();
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "insufficient Send did not fail: {row}"
            );
            tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
        };
        anyhow::ensure!(
            failed["id"] == admission.operation["id"]
                && failed["txid"] == ""
                && failed["last_error"]
                    .as_str()
                    .is_some_and(|s| s.contains("insufficient")),
            "insufficient Send lost its durable failure: {failed}"
        );
        fixture.restart_server().await?;
        let faucet = json!({
            "account_id": 1, "pool": "ironwood", "amount_zatoshi": 200_000_000,
            "idempotency_key": "fund-after-insufficient-send",
        });
        let _: Activity = confirmed_payment(
            &client,
            fixture.api_url(),
            "/api/v1/faucet",
            Some(&faucet),
            SEND_TIMEOUT,
        )
        .await?;
        let replay = admit_payment(&client, fixture.api_url(), "/api/v1/send", &body).await?;
        anyhow::ensure!(
            replay.operation == failed,
            "later funding changed the failed Send"
        );
        let mut new_body = body.clone();
        new_body["idempotency_key"] = json!("insufficient-send-new-key");
        let paid: Activity = confirmed_payment(
            &client,
            fixture.api_url(),
            "/api/v1/send",
            Some(&new_body),
            SEND_TIMEOUT,
        )
        .await?;
        anyhow::ensure!(
            paid.status == "confirmed" && paid.id != failed["id"],
            "new key did not make a distinct payment"
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
                .any(|a| a.id == 2 && a.ironwood_zatoshi == 600_000_000),
            "SDK destination balance does not show exactly one payout"
        );
        let still_failed: Vec<serde_json::Value> = request_json(
            &client,
            fixture.api_url(),
            &admission.location,
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        anyhow::ensure!(
            still_failed == vec![failed],
            "failed intent changed after new payment"
        );
        fixture.assert_running().await
    }
    .await;
    let cleanup = fixture.shutdown().await;
    scenario?;
    cleanup
}
