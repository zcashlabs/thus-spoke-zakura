use super::*;
use super::{
    API_READ_TIMEOUT, Activity, Address, AddressFaucetResult, BlockHeight, ChainInfo, Client,
    FaucetAccount, Instant, LocalNetwork, PathBuf, RECOVERY_POLL_INTERVAL, RegtestStack, Result,
    SEND_TIMEOUT, TransactionEvidence, TransparentAddress, assert_address_faucet_inclusion, json,
    preserve_scenario_failure, queued_retry_survives_restart, request_json, rpc,
};

pub(super) async fn queued_external_faucet_retry_survives_lost_response_and_restart() -> Result<()>
{
    queued_retry_survives_restart(true).await
}

pub(super) async fn address_faucet_retry_after_server_restart_pays_each_destination_once()
-> Result<()> {
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
            nu7: one,
        };
        let external =
            Address::Transparent(TransparentAddress::PublicKeyHash([42; 20])).encode(&network);
        for (address, key) in [
            (internal, "internal-restart-faucet"),
            (external, "external-restart-faucet"),
        ] {
            // Fail confirmation mining only after the prior payout's change
            // has an eligible anchor, so this exercises broadcast recovery.
            let _: Vec<String> = rpc(&client, fixture.node_url(), "generate", json!([1])).await?;
            fixture.proxy().reject_payment_mining(true)?;
            let body = json!({
                "address": address, "amount_zatoshi": 1_000_000,
                "idempotency_key": key,
            });
            let admission =
                admit_payment(&client, fixture.api_url(), "/api/v1/faucet/address", &body).await?;
            let first: AddressFaucetResult = serde_json::from_value(
                wait_payment_state(
                    &client,
                    fixture.api_url(),
                    &admission.location,
                    "broadcast",
                    Instant::now() + SEND_TIMEOUT,
                )
                .await?,
            )?;
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

            // Recovery after restart must not need another payment POST.
            fixture.restart_server().await?;
            fixture.proxy().reject_payment_mining(false)?;
            let confirmed: AddressFaucetResult = serde_json::from_value(
                wait_payment_state(
                    &client,
                    fixture.api_url(),
                    &admission.location,
                    "confirmed",
                    Instant::now() + SEND_TIMEOUT,
                )
                .await?,
            )?;
            anyhow::ensure!(
                confirmed.status == "confirmed" && confirmed.txid == first.txid,
                "background recovery changed the original payment"
            );
            anyhow::ensure!(
                confirmed.address == address && confirmed.amount_zatoshi == 1_000_000,
                "recovered payment changed its destination or amount"
            );
            let tx: serde_json::Value = rpc(
                &client,
                fixture.node_url(),
                "getrawtransaction",
                json!([first.txid, 1]),
            )
            .await?;
            anyhow::ensure!(
                tx["blockhash"].as_str() == confirmed.block_hash.as_deref(),
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

pub(super) async fn internal_address_faucets_record_confirmed_activity() -> Result<()> {
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
            let key = uuid::Uuid::new_v4().to_string();
            let response = address_faucet_request(&client, &fixture, address, &key).await?;
            let after = faucet_activities(&client, &fixture).await?;
            let row = new_faucet_activity(&before, &after)?;
            assert_eq!(row.kind, "faucet");
            assert_eq!(row.from_account, None);
            assert_eq!(row.to_account, Some(2));
            assert_eq!(row.source_pool, "ironwood");
            assert_eq!(row.destination_pool, pool);
            assert_eq!(row.amount_zatoshi, 100_000_000);
            assert_eq!(row.status, "confirmed");
            assert_eq!(Some(row.txid.as_str()), response["txid"].as_str());
            assert_eq!(row.block_hash.as_deref(), response["block_hash"].as_str());
            assert_address_faucet_inclusion(&client, &fixture, &response, address, &key).await?;
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

pub(super) async fn internal_address_faucet_recovers_after_auto_mine_failure() -> Result<()> {
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
        let _: serde_json::Value = confirmed_payment(
            &client,
            fixture.api_url(),
            "/api/v1/faucet/address",
            Some(&json!({"address": address, "amount_zatoshi": 1_000_000, "idempotency_key": "internal-address-warmup"})),
            SEND_TIMEOUT,
        )
        .await?;
        // Mature the warmup's treasury change before failing confirmation mining.
        let _: Vec<String> = rpc(&client, fixture.node_url(), "generate", json!([1])).await?;
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
        fixture.proxy().reject_payment_mining(true)?;
        let key = uuid::Uuid::new_v4().to_string();
        let admission = admit_payment(
            &client, fixture.api_url(), "/api/v1/faucet/address",
            &json!({"address": address, "amount_zatoshi": 100_000_000, "idempotency_key": key}),
        ).await?;
        let response: AddressFaucetResult = serde_json::from_value(wait_payment_state(
            &client, fixture.api_url(), &admission.location, "broadcast", Instant::now() + SEND_TIMEOUT,
        ).await?)?;
        assert!(fixture.proxy().counts().rejected > counts.rejected);
        assert_eq!(fixture.proxy().counts().forwarded, counts.forwarded);
        let failed_height: ChainInfo =
            rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
        assert_eq!(failed_height.blocks, height.blocks);
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
                let transparent_balance = accounts
                    .iter()
                    .find(|account| account.id == 2)
                    .unwrap()
                    .transparent_zatoshi;
                if transparent_balance == balance + 100_000_000 {
                    break;
                }
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

pub(super) async fn external_address_faucet_behavior_is_unchanged() -> Result<()> {
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
        let key = uuid::Uuid::new_v4().to_string();
        let response = address_faucet_request(&client, &fixture, &address, &key).await?;
        assert_address_faucet_inclusion(&client, &fixture, &response, &address, &key).await?;
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
    key: &str,
) -> Result<serde_json::Value> {
    confirmed_payment(
        client,
        fixture.api_url(),
        "/api/v1/faucet/address",
        Some(&json!({"address": address, "amount_zatoshi": 100_000_000, "idempotency_key": key})),
        SEND_TIMEOUT,
    )
    .await
}

pub(super) fn external_faucet_test_address() -> String {
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
        nu7: one,
    };
    Address::Transparent(transparent::address::TransparentAddress::PublicKeyHash(
        [0x7a; 20],
    ))
    .encode(&params)
}
