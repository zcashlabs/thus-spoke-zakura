use super::*;

const BASELINE_SOURCE: &str = "42dfcea09ee3cf696dcf6cd922bf370a10e9da77";

pub(super) async fn retained_rc7_payment_survives_common4_and_node_upgrade() -> Result<()> {
    let baseline = std::env::var_os("THS_UPGRADE_BASELINE_SERVER")
        .map(PathBuf::from)
        .context("set THS_UPGRADE_BASELINE_SERVER to ths-server built with --locked from 42dfcea09ee3cf696dcf6cd922bf370a10e9da77 (rc7/Common 2.2.0)")?;
    anyhow::ensure!(
        baseline.is_file(),
        "THS_UPGRADE_BASELINE_SERVER must name the source-built baseline executable"
    );
    let current = PathBuf::from(env!("CARGO_BIN_EXE_ths-server"));
    anyhow::ensure!(
        baseline.canonicalize()? != current.canonicalize()?,
        "baseline and upgraded server must be different binaries"
    );
    eprintln!(
        "upgrade baseline source={BASELINE_SOURCE} binary={} node=zakuracore/zakura:1.6.0; upgraded Cargo binary={} node=zakuracore/zakura:1.7.0",
        baseline.display(),
        current.display()
    );
    let mut fixture = RegtestStack::upgrade_baseline(baseline)?;
    let scenario = async {
        fixture.start().await?;
        let client = Client::new();
        let identities = retained_identities(fixture.data_dir())?;
        let before: Vec<AccountBalance> = request_json(&client, fixture.api_url(), "/api/v1/accounts", None, API_READ_TIMEOUT).await?;
        let key = "rc7-common4-retained-payment";
        let body = json!({"from_account":1,"to_account":2,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":1_000_000,"idempotency_key":key});
        fixture.proxy().reject_payment_mining(true)?;
        let lost = lose_payment_response(&client, fixture.api_url(), "/api/v1/send", &body).await?;
        let location = format!("/api/v1/activity?idempotency_key={key}");
        let pending = wait_payment_state(&client, fixture.api_url(), &location, "broadcast", Instant::now() + SEND_TIMEOUT).await?;
        let original = pending_payment_snapshot(&fixture, false, key)?;
        anyhow::ensure!(lost["id"] == original.id && pending["txid"] == original.txid, "baseline lost response changed the original intent");
        let mempool: Vec<String> = rpc(&client, fixture.node_url(), "getrawmempool", json!([])).await?;
        anyhow::ensure!(mempool.contains(&original.txid), "baseline payment never reached node 1.6.0");
        let checkpoint: ChainInfo = rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        let endpoints = (fixture.api_url().to_owned(), fixture.node_url().to_owned(), fixture.lightwalletd_port()?);

        fixture.upgrade_retained_stack(current).await?;
        anyhow::ensure!((fixture.api_url().to_owned(), fixture.node_url().to_owned(), fixture.lightwalletd_port()?) == endpoints, "upgrade replaced the selected fixture endpoints");
        anyhow::ensure!(retained_identities(fixture.data_dir())? == identities, "upgrade changed the seed or one of six account addresses");
        anyhow::ensure!(pending_payment_snapshot(&fixture, false, key)? == original, "upgrade changed original activity, txid, signed bytes, expiry, or input reservations");
        let upgraded_checkpoint: ChainInfo = rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        anyhow::ensure!(upgraded_checkpoint.blocks == checkpoint.blocks && upgraded_checkpoint.bestblockhash == checkpoint.bestblockhash, "replacement node did not retain the baseline chain");
        let replay = admit_payment(&client, fixture.api_url(), "/api/v1/send", &body).await?;
        anyhow::ensure!(replay.operation["id"] == original.id && replay.operation["txid"] == original.txid, "upgrade replay replaced the signed payment");
        fixture.proxy().reject_payment_mining(false)?;
        let confirmed = wait_payment_state(&client, fixture.api_url(), &location, "confirmed", Instant::now() + SEND_TIMEOUT).await?;
        anyhow::ensure!(confirmed["id"] == original.id && confirmed["txid"] == original.txid, "upgrade recovery confirmed a replacement");
        let evidence: serde_json::Value = rpc(&client, fixture.node_url(), "getrawtransaction", json!([original.txid, 1])).await?;
        anyhow::ensure!(evidence["confirmations"].as_u64().is_some_and(|n| n > 0) && evidence["blockhash"] == confirmed["block_hash"], "upgraded payment lacks chain inclusion evidence");
        let block: serde_json::Value = rpc(&client, fixture.node_url(), "getblock", json!([evidence["blockhash"], 1])).await?;
        let canonical: String = rpc(&client, fixture.node_url(), "getblockhash", json!([block["height"]])).await?;
        anyhow::ensure!(evidence["blockhash"].as_str() == Some(canonical.as_str()) && block["tx"].as_array().context("confirming block omitted transactions")?.iter().filter(|txid| **txid == json!(original.txid)).count() == 1, "original transaction was not included once on the canonical chain");
        let raw: String = rpc(&client, fixture.node_url(), "getrawtransaction", json!([original.txid, 0])).await?;
        anyhow::ensure!(hex::decode(raw)? == original.raw, "chain transaction differs from baseline signed bytes");
        let prepared = prepared_payment(fixture.data_dir(), &original.id)?.context("migrated wallet lost original prepared payment")?;
        anyhow::ensure!(prepared.txid == original.txid && prepared.raw_transaction == original.raw && prepared.expiry_height == original.expiry_height, "confirmation changed the old wallet journal");
        let store = Connection::open_with_flags(fixture.data_dir().join("ths.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let server_prepared: (Vec<u8>, u64) = store.query_row("SELECT raw_transaction,expiry_height FROM prepared_payments WHERE activity_id=?1", [&original.id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        anyhow::ensure!(server_prepared == (original.raw.clone(), original.expiry_height), "confirmation changed the old server journal");
        let operations: u64 = store.query_row("SELECT COUNT(*) FROM activity a JOIN idempotency i ON i.activity_id=a.id WHERE i.key=?1", [key], |row| row.get(0))?;
        anyhow::ensure!(operations == 1, "upgrade replay created duplicate activity");
        drop(store);
        let fee = evidence["ironwood"]["valueBalanceZat"].as_u64().context("shielded transfer omitted its fee balance")?;
        let deadline = Instant::now() + SEND_TIMEOUT;
        let reconciled = loop {
            let accounts: Vec<AccountBalance> = request_json(&client, fixture.api_url(), "/api/v1/accounts", None, API_READ_TIMEOUT).await?;
            anyhow::ensure!(accounts.iter().map(|a| a.id).collect::<Vec<_>>() == vec![1,2,3,4,5], "upgrade exposed the treasury or lost user accounts");
            let balances_match = accounts.iter().all(|account| {
                before.iter().find(|old| old.id == account.id).is_some_and(|old| {
                    let expected = match account.id {
                        1 => old.ironwood_zatoshi.checked_sub(1_000_000 + fee),
                        2 => old.ironwood_zatoshi.checked_add(1_000_000),
                        _ => Some(old.ironwood_zatoshi),
                    };
                    expected == Some(account.ironwood_zatoshi) && old.transparent_zatoshi == account.transparent_zatoshi
                })
            });
            if balances_match { break accounts; }
            anyhow::ensure!(Instant::now() < deadline, "upgraded wallet balances did not reconcile to one payout and its fee");
            tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
        };
        let tip: ChainInfo = rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        let final_replay = admit_payment(&client, fixture.api_url(), "/api/v1/send", &body).await?.operation;
        anyhow::ensure!(final_replay == confirmed, "confirmed replay changed the migrated operation");
        let after: ChainInfo = rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        anyhow::ensure!(after.blocks == tip.blocks && after.bestblockhash == tip.bestblockhash, "confirmed replay mined a second payout");
        let accounts: Vec<AccountBalance> = request_json(&client, fixture.api_url(), "/api/v1/accounts", None, API_READ_TIMEOUT).await?;
        anyhow::ensure!(accounts.len() == reconciled.len() && accounts.iter().zip(&reconciled).all(|(a,b)| a.id == b.id && a.ironwood_zatoshi == b.ironwood_zatoshi && a.transparent_zatoshi == b.transparent_zatoshi), "confirmed replay changed reconciled balances");
        let mempool: Vec<String> = rpc(&client, fixture.node_url(), "getrawmempool", json!([])).await?;
        anyhow::ensure!(mempool.is_empty(), "confirmed replay submitted a second transaction");
        let fresh: serde_json::Value = confirmed_payment(
            &client, fixture.api_url(), "/api/v1/send",
            Some(&json!({"from_account":2,"to_account":3,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":100_000,"idempotency_key":"fresh-payment-after-rc7-upgrade"})),
            SEND_TIMEOUT,
        ).await?;
        anyhow::ensure!(fresh["txid"].as_str().is_some_and(|txid| txid != original.txid), "upgraded wallet did not construct a new transaction");
        let fresh_evidence: serde_json::Value = rpc(&client, fixture.node_url(), "getrawtransaction", json!([fresh["txid"], 1])).await?;
        anyhow::ensure!(fresh_evidence["confirmations"].as_u64().is_some_and(|n| n > 0) && fresh_evidence["blockhash"] == fresh["block_hash"], "new payment from migrated keys did not reach the chain");
        let original_after = prepared_payment(fixture.data_dir(), &original.id)?.context("new payment removed the old recovery journal")?;
        anyhow::ensure!(original_after.txid == original.txid && original_after.raw_transaction == original.raw && original_after.expiry_height == original.expiry_height, "new payment mutated retained signed work");
        fixture.assert_running().await
    }.await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

#[derive(PartialEq, Eq)]
struct RetainedIdentities {
    seed: String,
    accounts: Vec<(u8, String, String)>,
}

fn retained_identities(path: &std::path::Path) -> Result<RetainedIdentities> {
    let store = Connection::open_with_flags(path.join("ths.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let seed = store.query_row("SELECT value FROM metadata WHERE key='seed'", [], |row| {
        row.get(0)
    })?;
    let mut statement =
        store.prepare("SELECT id,unified_address,transparent_address FROM accounts ORDER BY id")?;
    let accounts = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    anyhow::ensure!(
        accounts.iter().map(|(id, _, _)| *id).collect::<Vec<u8>>() == vec![1, 2, 3, 4, 5, 6],
        "retained store must contain six account identities"
    );
    Ok(RetainedIdentities { seed, accounts })
}
