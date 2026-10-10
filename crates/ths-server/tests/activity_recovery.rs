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
async fn queued_send_retry_survives_lost_response_and_restart() -> Result<()> {
    queued_retry_survives_restart(false).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn queued_external_faucet_retry_survives_lost_response_and_restart() -> Result<()> {
    queued_retry_survives_restart(true).await
}

#[derive(PartialEq, Eq)]
struct PendingPaymentSnapshot {
    id: String,
    txid: String,
    raw: Vec<u8>,
    expiry_height: u64,
    reserved_inputs: Vec<(String, i64)>,
}

fn pending_payment_snapshot(
    fixture: &RegtestStack,
    external: bool,
    key: &str,
) -> Result<PendingPaymentSnapshot> {
    let db = Connection::open_with_flags(
        fixture.data_dir().join("ths.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let sql = if external {
        "SELECT id,txid,status,block_hash FROM address_faucets WHERE key=?1"
    } else {
        "SELECT a.id,a.txid,a.status,a.block_hash FROM activity a JOIN idempotency i ON i.activity_id=a.id WHERE i.key=?1"
    };
    let (id, txid, status, block_hash): (String, String, String, Option<String>) =
        db.query_row(sql, [key], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
    anyhow::ensure!(
        status == "broadcast" && block_hash.is_none(),
        "payment is not pending without a block hash"
    );
    let (raw, expiry_height): (Vec<u8>, u64) = db.query_row(
        "SELECT raw_transaction,expiry_height FROM prepared_payments WHERE activity_id=?1",
        [&id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let wallet = Connection::open_with_flags(
        fixture.data_dir().join("wallet.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let (wallet_txid, wallet_raw, wallet_expiry): (String, Vec<u8>, u64) = wallet.query_row("SELECT txid,raw_transaction,expiry_height FROM ext_tsz_prepared_payments WHERE activity_id=?1", [&id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
    anyhow::ensure!(
        wallet_txid == txid && wallet_raw == raw && wallet_expiry == expiry_height,
        "wallet and server prepared payment journals differ"
    );
    // Compare reservation associations by transaction bytes, avoiding display-endian txid conversion.
    let mut statement = wallet.prepare(
        "SELECT 'ironwood',s.ironwood_received_note_id FROM ironwood_received_note_spends s JOIN transactions t ON t.id_tx=s.transaction_id WHERE t.raw=?1
         UNION ALL SELECT 'transparent',s.transparent_received_output_id FROM transparent_received_output_spends s JOIN transactions t ON t.id_tx=s.transaction_id WHERE t.raw=?1 ORDER BY 1,2")?;
    let reserved_inputs = statement
        .query_map([&raw], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    anyhow::ensure!(
        !reserved_inputs.is_empty(),
        "original transaction has no reserved wallet inputs"
    );
    Ok(PendingPaymentSnapshot {
        id,
        txid,
        raw,
        expiry_height,
        reserved_inputs,
    })
}

async fn queued_retry_survives_restart(external: bool) -> Result<()> {
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
        fixture.proxy().fail_next_generate()?;
        let lost = lose_payment_response(&client, fixture.api_url(), path, &body).await?;
        anyhow::ensure!(lost["status"] == "broadcast" && lost["block_hash"].is_null(), "first broadcast did not remain pending");
        let original = pending_payment_snapshot(&fixture, external, key)?;
        anyhow::ensure!(lost["txid"].as_str() == Some(original.txid.as_str()), "lost response and journals disagree");
        let mempool: Vec<String> = rpc(&client, fixture.node_url(), "getrawmempool", json!([])).await?;
        anyhow::ensure!(mempool.contains(&original.txid), "original broadcast did not reach the real node");

        fixture.proxy().hide_transaction(Some(original.txid.clone()))?;
        fixture.proxy().fail_next_generate()?;
        proxy.queue_next_broadcast();
        let retry: serde_json::Value = request_json(&client, fixture.api_url(), path, Some(&body), SEND_TIMEOUT).await?;
        anyhow::ensure!(retry == lost, "queued retry changed the original pending response");
        proxy.assert_identical_retry()?;
        anyhow::ensure!(fixture.proxy().hidden_lookups()? > 0, "queued retry did not exercise missing transaction lookup");
        anyhow::ensure!(pending_payment_snapshot(&fixture, external, key)? == original, "queued retry changed journals or reserved inputs");

        fixture.restart_server().await?;
        anyhow::ensure!(pending_payment_snapshot(&fixture, external, key)? == original, "reopening stores changed the pending original payment");
        fixture.proxy().hide_transaction(None)?;
        let _: serde_json::Value = rpc(&client, fixture.node_url(), "generate", json!([1])).await?;
        let confirmed: serde_json::Value = request_json(&client, fixture.api_url(), path, Some(&body), SEND_TIMEOUT).await?;
        anyhow::ensure!(confirmed["status"] == "confirmed" && confirmed["txid"].as_str() == Some(original.txid.as_str()), "retry did not confirm the original transaction");
        let evidence: serde_json::Value = rpc(&client, fixture.node_url(), "getrawtransaction", json!([original.txid, 1])).await?;
        anyhow::ensure!(evidence["confirmations"].as_u64().is_some_and(|n| n > 0) && evidence["blockhash"].is_string() && confirmed["block_hash"] == evidence["blockhash"], "confirmation lacks canonical transaction evidence");
        let block: serde_json::Value = rpc(&client, fixture.node_url(), "getblockheader", json!([evidence["blockhash"]])).await?;
        let canonical: String = rpc(&client, fixture.node_url(), "getblockhash", json!([block["height"]])).await?;
        anyhow::ensure!(evidence["blockhash"].as_str() == Some(canonical.as_str()), "confirmed transaction is not on the canonical chain");
        let before: ChainInfo = rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        let replay: serde_json::Value = request_json(&client, fixture.api_url(), path, Some(&body), SEND_TIMEOUT).await?;
        anyhow::ensure!(replay == confirmed, "confirmed replay changed the payment");
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

const RECOVERY_IDEMPOTENCY_KEY: &str = "recovery-after-auto-mine-failure";
const CONCURRENT_IDEMPOTENCY_KEY: &str = "concurrent-identical-send";
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
    ironwood_zatoshi: u64,
    transparent_zatoshi: u64,
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
    let server = PathBuf::from(env!("CARGO_BIN_EXE_ths-server"));
    let mut fixture = RegtestStack::new(server)?;
    let scenario = async {
        fixture.start().await?;
        let client = Client::new();
        let accounts: serde_json::Value = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/accounts",
            None,
            API_READ_TIMEOUT,
        )
        .await?;
        let internal = accounts
            .as_array()
            .and_then(|accounts| accounts.iter().find(|account| account["id"] == 2))
            .and_then(|account| account["transparent_address"].as_str())
            .ok_or_else(|| anyhow::anyhow!("Account 2 transparent address is missing"))?
            .to_owned();
        let one = Some(BlockHeight::from_u32(1));
        let network = LocalNetwork {
            overwinter: one,
            sapling: one,
            blossom: one,
            heartwood: one,
            canopy: one,
            nu5: one,
            nu6: one,
            nu6_1: one,
            nu6_2: one,
            nu6_3: one,
            nu7: None,
        };
        let external =
            Address::Transparent(TransparentAddress::PublicKeyHash([42; 20])).encode(&network);
        for (address, key) in [
            (internal, "internal-restart-faucet"),
            (external, "external-restart-faucet"),
        ] {
            fixture.proxy().fail_next_generate()?;
            let body = json!({
                "address": address, "amount_zatoshi": 1_000_000,
                "idempotency_key": key,
            });
            let first: AddressFaucetResult = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/faucet/address",
                Some(&body),
                SEND_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(
                first.status == "broadcast" && first.block_hash.is_none(),
                "injected auto-mine failure did not leave the payment pending"
            );
            let mempool: Vec<String> =
                rpc(&client, fixture.node_url(), "getrawmempool", json!([])).await?;
            anyhow::ensure!(
                mempool.contains(&first.txid),
                "original transaction is absent from the mempool"
            );

            // Discard the first result as a client with a lost response would, then restart the app.
            fixture.restart_server().await?;
            let replay: AddressFaucetResult = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/faucet/address",
                Some(&body),
                SEND_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(
                replay.status == "confirmed" && replay.txid == first.txid,
                "retry constructed another payment instead of confirming the original"
            );
            anyhow::ensure!(
                replay.address == address && replay.amount_zatoshi == 1_000_000,
                "replayed payment changed its destination or amount"
            );
            let tx: serde_json::Value = rpc(
                &client,
                fixture.node_url(),
                "getrawtransaction",
                json!([first.txid, 1]),
            )
            .await?;
            anyhow::ensure!(
                tx["blockhash"].as_str() == replay.block_hash.as_deref(),
                "response block hash differs from the confirmed transaction"
            );
            let balance: serde_json::Value = rpc(
                &client,
                fixture.node_url(),
                "getaddressbalance",
                json!([{"addresses": [address]}]),
            )
            .await?;
            anyhow::ensure!(
                balance["balance"] == 1_000_000,
                "destination received more or less than one intended payout"
            );
        }
        fixture.assert_running().await?;
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn same_account_cross_pool_round_trip_is_replay_safe() -> Result<()> {
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
            let sent: Activity = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/send",
                Some(&body),
                SEND_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(sent.status == "confirmed", "send did not confirm");
            anyhow::ensure!(sent.from_account == Some(1), "source account changed");
            anyhow::ensure!(sent.to_account == 1, "destination account changed");
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
            let replay: Activity = request_json(
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

#[derive(Debug, Deserialize)]
struct FaucetAccount {
    id: u8,
    transparent_address: String,
    unified_address: String,
    transparent_zatoshi: u64,
    ironwood_zatoshi: u64,
}

async fn faucet_accounts(client: &Client, fixture: &RegtestStack) -> Result<Vec<FaucetAccount>> {
    request_json(
        client,
        fixture.api_url(),
        "/api/v1/accounts",
        None,
        API_READ_TIMEOUT,
    )
    .await
}

async fn faucet_activities(client: &Client, fixture: &RegtestStack) -> Result<Vec<Activity>> {
    request_json(
        client,
        fixture.api_url(),
        "/api/v1/activity?limit=100",
        None,
        API_READ_TIMEOUT,
    )
    .await
}

fn new_faucet_activity<'a>(before: &[Activity], after: &'a [Activity]) -> Result<&'a Activity> {
    let added: Vec<_> = after
        .iter()
        .filter(|row| !before.iter().any(|old| old.id == row.id))
        .collect();
    anyhow::ensure!(added.len() == 1, "expected exactly one new faucet activity");
    Ok(added[0])
}

async fn address_faucet_request(
    client: &Client,
    fixture: &RegtestStack,
    address: &str,
) -> Result<serde_json::Value> {
    request_json(
        client,
        fixture.api_url(),
        "/api/v1/faucet/address",
        Some(&json!({"address": address, "amount_zatoshi": 100_000_000, "idempotency_key": uuid::Uuid::new_v4().to_string()})),
        SEND_TIMEOUT,
    )
    .await
}

async fn assert_address_faucet_inclusion(
    client: &Client,
    fixture: &RegtestStack,
    response: &serde_json::Value,
    address: &str,
) -> Result<()> {
    anyhow::ensure!(
        response.as_object().is_some_and(|fields| fields.len() == 5),
        "address faucet response fields changed"
    );
    anyhow::ensure!(
        response["address"] == address
            && response["amount_zatoshi"] == 100_000_000
            && response["status"] == "confirmed",
        "address faucet destination, amount, or confirmation status changed"
    );
    let evidence: TransactionEvidence = rpc(
        client,
        fixture.node_url(),
        "getrawtransaction",
        json!([response["txid"], 1]),
    )
    .await?;
    anyhow::ensure!(
        evidence.confirmations.is_some_and(|count| count > 0),
        "faucet transaction is not confirmed"
    );
    anyhow::ensure!(
        evidence.blockhash.as_deref() == response["block_hash"].as_str(),
        "faucet inclusion hash differs from response"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn internal_address_faucets_record_confirmed_activity() -> Result<()> {
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    let scenario = async {
        fixture.start().await?;
        fixture.assert_running().await?;
        let client = Client::new();
        let accounts = faucet_accounts(&client, &fixture).await?;
        let account = accounts.iter().find(|account| account.id == 2).unwrap();
        let mut payments = Vec::new();
        for (address, pool) in [
            (&account.transparent_address, "transparent"),
            (&account.unified_address, "ironwood"),
            (&account.transparent_address, "transparent"),
        ] {
            let before = faucet_activities(&client, &fixture).await?;
            let response = address_faucet_request(&client, &fixture, address).await?;
            let after = faucet_activities(&client, &fixture).await?;
            let row = new_faucet_activity(&before, &after)?;
            assert_eq!(row.kind, "faucet");
            assert_eq!(row.from_account, None);
            assert_eq!(row.to_account, 2);
            assert_eq!(row.source_pool, "ironwood");
            assert_eq!(row.destination_pool, pool);
            assert_eq!(row.amount_zatoshi, 100_000_000);
            assert_eq!(row.status, "confirmed");
            assert_eq!(Some(row.txid.as_str()), response["txid"].as_str());
            assert_eq!(row.block_hash.as_deref(), response["block_hash"].as_str());
            assert_address_faucet_inclusion(&client, &fixture, &response, address).await?;
            payments.push((row.id.clone(), row.txid.clone()));
        }
        assert_ne!(payments[0].0, payments[2].0);
        assert_ne!(payments[0].1, payments[2].1);
        let after = faucet_accounts(&client, &fixture).await?;
        let after = after.iter().find(|account| account.id == 2).unwrap();
        assert_eq!(
            after.transparent_zatoshi,
            account.transparent_zatoshi + 200_000_000
        );
        assert_eq!(
            after.ironwood_zatoshi,
            account.ironwood_zatoshi + 100_000_000
        );
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn internal_address_faucet_recovers_after_auto_mine_failure() -> Result<()> {
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    let scenario = async {
        fixture.start().await?;
        fixture.assert_running().await?;
        let client = Client::new();
        let accounts = faucet_accounts(&client, &fixture).await?;
        let address = &accounts
            .iter()
            .find(|account| account.id == 2)
            .unwrap()
            .transparent_address;
        let _: serde_json::Value = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/faucet/address",
            Some(&json!({"address": address, "amount_zatoshi": 1_000_000, "idempotency_key": "internal-address-warmup"})),
            SEND_TIMEOUT,
        )
        .await?;
        let warmed = faucet_accounts(&client, &fixture).await?;
        let balance = warmed
            .iter()
            .find(|account| account.id == 2)
            .unwrap()
            .transparent_zatoshi;
        let before = faucet_activities(&client, &fixture).await?;
        let height: ChainInfo =
            rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        let counts = fixture.proxy().counts();
        fixture.proxy().fail_next_generate()?;
        let response = client
            .post(format!("{}/api/v1/faucet/address", fixture.api_url()))
            .timeout(SEND_TIMEOUT)
            .json(&json!({"address": address, "amount_zatoshi": 100_000_000, "idempotency_key": uuid::Uuid::new_v4().to_string()}))
            .send()
            .await?;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::OK
        );
        assert_eq!(
            fixture.proxy().counts(),
            GenerateCounts {
                rejected: counts.rejected + 1,
                forwarded: counts.forwarded
            }
        );
        let failed_height: ChainInfo =
            rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        assert_eq!(failed_height.blocks, height.blocks);
        let response: AddressFaucetResult = response.json().await?;
        assert_eq!(response.status, "broadcast");
        assert_eq!(response.block_hash, None);
        let after = faucet_activities(&client, &fixture).await?;
        let pending = new_faucet_activity(&before, &after)?.clone();
        assert_eq!(pending.status, "broadcast");
        assert_eq!(pending.block_hash, None);
        let mempool: Vec<String> =
            rpc(&client, fixture.node_url(), "getrawmempool", json!([])).await?;
        assert!(mempool.contains(&pending.txid));
        let _: Vec<String> = rpc(&client, fixture.node_url(), "generate", json!([1])).await?;
        let evidence: TransactionEvidence = rpc(
            &client,
            fixture.node_url(),
            "getrawtransaction",
            json!([pending.txid, 1]),
        )
        .await?;
        assert!(evidence.confirmations.is_some_and(|count| count > 0));
        let deadline = RegtestStack::recovery_deadline();
        loop {
            let activities: Vec<Activity> = fixture
                .recovery_read(deadline, "/api/v1/activity?limit=100")
                .await?;
            let row = new_faucet_activity(&before, &activities)?;
            assert_eq!(row.id, pending.id);
            assert_eq!(row.txid, pending.txid);
            if row.status == "confirmed" {
                assert_eq!(row.block_hash, evidence.blockhash);
                let accounts: Vec<FaucetAccount> =
                    fixture.recovery_read(deadline, "/api/v1/accounts").await?;
                assert_eq!(
                    accounts
                        .iter()
                        .find(|account| account.id == 2)
                        .unwrap()
                        .transparent_zatoshi,
                    balance + 100_000_000
                );
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "address faucet activity did not recover"
            );
            tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
        }
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

fn external_faucet_test_address() -> String {
    use zcash_keys::address::Address;
    use zcash_protocol::{consensus::BlockHeight, local_consensus::LocalNetwork};
    let one = Some(BlockHeight::from_u32(1));
    let params = LocalNetwork {
        overwinter: one,
        sapling: one,
        blossom: one,
        heartwood: one,
        canopy: one,
        nu5: one,
        nu6: one,
        nu6_1: one,
        nu6_2: one,
        nu6_3: one,
        nu7: None,
    };
    Address::Transparent(transparent::address::TransparentAddress::PublicKeyHash(
        [0x7a; 20],
    ))
    .encode(&params)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn external_address_faucet_behavior_is_unchanged() -> Result<()> {
    let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
    let scenario = async {
        fixture.start().await?;
        fixture.assert_running().await?;
        let client = Client::new();
        let address = external_faucet_test_address();
        let accounts = faucet_accounts(&client, &fixture).await?;
        assert!(
            accounts
                .iter()
                .all(|account| account.transparent_address != address
                    && account.unified_address != address)
        );
        let before = faucet_activities(&client, &fixture).await?;
        let response = address_faucet_request(&client, &fixture, &address).await?;
        assert_address_faucet_inclusion(&client, &fixture, &response, &address).await?;
        let after = faucet_activities(&client, &fixture).await?;
        assert_eq!(
            before.iter().map(|row| &row.id).collect::<Vec<_>>(),
            after.iter().map(|row| &row.id).collect::<Vec<_>>()
        );
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, cleanup)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn concurrent_identical_sends_have_one_chain_effect() -> Result<()> {
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn broadcast_recovers_after_auto_mine_failure() -> Result<()> {
    run_live_scenario(LiveScenario::BroadcastRecovery).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and prepared regtest images"]
async fn large_reward_history_keeps_treasury_faucet_responsive() -> Result<()> {
    run_live_scenario(LiveScenario::TreasurySync).await
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

async fn exercise_treasury_sync(
    fixture: &mut RegtestStack,
    reporter: &mut RecoveryFailureReporter,
) -> Result<()> {
    let client = Client::new();
    let before: ChainInfo =
        rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;

    reporter.phase(RecoveryPhase::DirectMine);
    let _: Vec<String> = rpc_with_timeout(
        &client,
        fixture.node_url(),
        "generate",
        json!([10_000]),
        LARGE_MINE_TIMEOUT,
    )
    .await?;
    let bulk_tip: ChainInfo =
        rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
    anyhow::ensure!(
        bulk_tip.blocks == before.blocks + 10_000,
        "direct mining did not create the expected reward history"
    );
    reporter.record_height(HeightCheckpoint::Tip, bulk_tip.blocks);

    reporter.phase(RecoveryPhase::Recovery);
    wait_for_wallet_height(fixture, reporter, bulk_tip.blocks, LARGE_MINE_TIMEOUT).await?;

    // An additional block tests refresh cost after the large reward history exists.
    let _: Vec<String> = rpc(&client, fixture.node_url(), "generate", json!([1])).await?;
    let incremental_tip: ChainInfo =
        rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
    anyhow::ensure!(
        incremental_tip.blocks == bulk_tip.blocks + 1,
        "incremental mining did not advance the chain by one block"
    );
    reporter.record_height(HeightCheckpoint::Tip, incremental_tip.blocks);
    wait_for_wallet_height(fixture, reporter, incremental_tip.blocks, SEND_TIMEOUT).await?;

    reporter.phase(RecoveryPhase::Faucet);
    let accounts: Vec<serde_json::Value> = request_json(
        &client,
        fixture.api_url(),
        "/api/v1/accounts",
        None,
        API_READ_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(
        accounts.len() == 5
            && accounts.iter().all(|account| {
                account["id"]
                    .as_u64()
                    .is_some_and(|id| (1..=5).contains(&id))
            }),
        "treasury appeared in public accounts"
    );
    let initial_balance = accounts
        .iter()
        .find(|account| account["id"] == 2)
        .and_then(|account| account["ironwood_zatoshi"].as_u64())
        .ok_or_else(|| anyhow::anyhow!("destination balance was missing"))?;
    for request in 0..10 {
        let payment: Activity = request_json(
            &client,
            fixture.api_url(),
            "/api/v1/faucet",
            Some(&json!({
                "account_id": 2,
                "pool": "ironwood",
                "amount_zatoshi": 500_000_000u64,
                "idempotency_key": format!("treasury-history-faucet-{request}"),
            })),
            SEND_TIMEOUT,
        )
        .await?;
        anyhow::ensure!(
            payment.status == "confirmed" && payment.amount_zatoshi == 500_000_000,
            "faucet payment did not confirm after large reward history"
        );
    }
    let accounts: Vec<serde_json::Value> = request_json(
        &client,
        fixture.api_url(),
        "/api/v1/accounts",
        None,
        API_READ_TIMEOUT,
    )
    .await?;
    let final_balance = accounts
        .iter()
        .find(|account| account["id"] == 2)
        .and_then(|account| account["ironwood_zatoshi"].as_u64())
        .ok_or_else(|| anyhow::anyhow!("final destination balance was missing"))?;
    anyhow::ensure!(
        final_balance == initial_balance + 5_000_000_000,
        "faucet did not deliver ten maximum payments"
    );
    Ok(())
}

async fn wait_for_wallet_height(
    fixture: &mut RegtestStack,
    reporter: &mut RecoveryFailureReporter,
    height: u64,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = match fixture
            .recovery_read::<Status>(deadline, "/api/v1/status")
            .await
        {
            Ok(status) => status,
            Err(error) if support::is_retryable_read_transport(&error) => {
                tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        if let Some(scanned) = status.wallet_sync.fully_scanned_height {
            reporter.record_height(HeightCheckpoint::Scanned, scanned);
        }
        if status.wallet_sync.state == "ready"
            && status.wallet_sync.fully_scanned_height == Some(height)
        {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "wallet did not converge after direct mining"
        );
        tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
    }
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
            "source_pool": "ironwood",
            "destination_pool": "ironwood",
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
        fixture.data_dir().join("ths.db"),
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
        activity.source_pool == "ironwood",
        "activity source pool changed"
    );
    anyhow::ensure!(
        activity.destination_pool == "ironwood",
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
            source_pool: "ironwood".to_owned(),
            destination_pool: "ironwood".to_owned(),
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
