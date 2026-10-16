use super::*;

pub(super) async fn reorg_and_rollback_preserve_canonical_wallet_and_payment_state() -> Result<()> {
    for keep_txs in [false, true] {
        let mut fixture = RegtestStack::new(PathBuf::from(env!("CARGO_BIN_EXE_ths-server")))?;
        let mut broadcast_proxy = None;
        let scenario = async {
            fixture.start().await?;
            broadcast_proxy =
                Some(QueuedBroadcastProxy::start(fixture.lightwalletd_port()?).await?);
            let proxy = broadcast_proxy.as_ref().unwrap();
            fixture.restart_with_lightwalletd(proxy.port()).await?;
            let client = Client::new();
            let ancestor_accounts: Vec<serde_json::Value> = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/accounts",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            for account in &ancestor_accounts {
                anyhow::ensure!(
                    account["transparent_zatoshi"] == 0
                        && account["ironwood_zatoshi"]
                            == if account["id"] == 1 { 500_000_000 } else { 0 },
                    "unexpected retained-ancestor balances: {ancestor_accounts:?}"
                );
            }
            let body = json!({"from_account":1,"to_account":2,"source_pool":"ironwood",
                "destination_pool":"ironwood","amount_zatoshi":1_000_000,
                "idempotency_key":"reorg-original-payment"});
            let original: Activity = confirmed_payment(
                &client,
                fixture.api_url(),
                "/api/v1/send",
                Some(&body),
                SEND_TIMEOUT,
            )
            .await
            .context("confirming original payment")?;
            let original_broadcasts = proxy.broadcast_counts();
            anyhow::ensure!(
                original_broadcasts.0 > 0,
                "original broadcast was not observed"
            );
            let original_transactions = constructed_transactions(fixture.data_dir())?;
            let original_journal = prepared_payment_journal(fixture.data_dir())?;
            let _: serde_json::Value = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/mine",
                Some(&json!({"blocks":2})),
                SEND_TIMEOUT,
            )
            .await
            .context("mining original suffix")?;
            let before: ChainInfo =
                rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
            let ancestor: String = rpc(
                &client,
                fixture.node_url(),
                "getblockhash",
                json!([before.blocks - 3]),
            )
            .await?;
            let balances: Vec<serde_json::Value> = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/accounts",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            let result: serde_json::Value = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/reorg",
                Some(&json!({"depth":3,"keep_txs":keep_txs})),
                SEND_TIMEOUT,
            )
            .await
            .context("reorging original suffix")?;
            let broadcasts_after_reorg = proxy.broadcast_counts();
            anyhow::ensure!(
                keep_txs || broadcasts_after_reorg == original_broadcasts,
                "reorg without keep_txs rebroadcast a payment"
            );
            anyhow::ensure!(
                result["old_tip"]["height"] == before.blocks
                    && result["new_tip"]["height"] == before.blocks,
                "reorg changed the tip height"
            );
            let removed = result["removed_hashes"]
                .as_array()
                .context("missing removed hashes")?;
            let replacements = result["replacement_hashes"]
                .as_array()
                .context("missing replacement hashes")?;
            anyhow::ensure!(
                removed.len() == 3
                    && replacements.len() == 3
                    && removed
                        .iter()
                        .zip(replacements)
                        .all(|(old, new)| old != new),
                "reorg did not replace exactly three hashes"
            );
            let current_ancestor: String = rpc(
                &client,
                fixture.node_url(),
                "getblockhash",
                json!([before.blocks - 3]),
            )
            .await?;
            anyhow::ensure!(
                current_ancestor == ancestor,
                "reorg changed the retained ancestor"
            );
            let status: serde_json::Value = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/status",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(
                status["settled"] == true,
                "reorg did not settle node, index and wallet"
            );
            let current: Vec<Activity> = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/activity?idempotency_key=reorg-original-payment",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(
                current.len() == 1
                    && current[0].id == original.id
                    && current[0].txid == original.txid,
                "reorg replaced the original payment identity"
            );
            if keep_txs {
                anyhow::ensure!(
                    result["failed_transactions"]
                        .as_array()
                        .is_some_and(Vec::is_empty),
                    "valid anchored transaction was not retained: {result}"
                );
                anyhow::ensure!(
                    current[0].status == "confirmed"
                        && current[0].block_hash != original.block_hash,
                    "retained payment kept its old confirmation"
                );
                let tx: serde_json::Value = rpc(
                    &client,
                    fixture.node_url(),
                    "getrawtransaction",
                    json!([original.txid, 1]),
                )
                .await?;
                anyhow::ensure!(
                    tx["confirmations"].as_i64().is_some_and(|count| count > 0)
                        && tx["blockhash"] == json!(current[0].block_hash),
                    "retained payment is not canonically included"
                );
                let after: Vec<serde_json::Value> = request_json(
                    &client,
                    fixture.api_url(),
                    "/api/v1/accounts",
                    None,
                    API_READ_TIMEOUT,
                )
                .await?;
                anyhow::ensure!(
                    after == balances,
                    "retained payment changed canonical user balances"
                );
            } else {
                anyhow::ensure!(
                    current[0].status == "unconfirmed" && current[0].block_hash.is_none(),
                    "dropped payment still reported confirmation"
                );
                let after: Vec<serde_json::Value> = request_json(
                    &client,
                    fixture.api_url(),
                    "/api/v1/accounts",
                    None,
                    API_READ_TIMEOUT,
                )
                .await?;
                anyhow::ensure!(
                    after == ancestor_accounts,
                    "reorg retained orphaned payment balance effects: {after:?}"
                );
            }

            let rollback: serde_json::Value = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/rollback",
                Some(&json!({"depth":3})),
                SEND_TIMEOUT,
            )
            .await
            .context("rolling back replacement suffix")?;
            anyhow::ensure!(
                rollback["new_tip"]["height"] == before.blocks - 3
                    && rollback["new_tip"]["hash"] == ancestor
                    && rollback["replacement_hashes"]
                        .as_array()
                        .is_some_and(Vec::is_empty),
                "rollback mined replacements"
            );
            let accounts: Vec<serde_json::Value> = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/accounts",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(
                accounts == ancestor_accounts,
                "rollback did not restore retained-ancestor balances: {accounts:?}"
            );
            let replay: Activity = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/send",
                Some(&body),
                API_READ_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(
                replay.id == original.id
                    && replay.txid == original.txid
                    && replay.status == "unconfirmed"
                    && replay.block_hash.is_none(),
                "replay revived an orphaned payment"
            );
            fixture.restart_server().await?;
            tokio::time::sleep(Duration::from_secs(5)).await;
            let after: ChainInfo =
                rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
            anyhow::ensure!(
                after.blocks == before.blocks - 3 && after.bestblockhash == ancestor,
                "background recovery mined after rollback/restart"
            );
            let status: serde_json::Value = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/status",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(status["settled"] == true, "rollback did not settle");
            let accounts: Vec<serde_json::Value> = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/accounts",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(
                accounts == ancestor_accounts,
                "restart did not preserve retained-ancestor balances: {accounts:?}"
            );
            let saved: Vec<Activity> = request_json(
                &client,
                fixture.api_url(),
                "/api/v1/activity?idempotency_key=reorg-original-payment",
                None,
                API_READ_TIMEOUT,
            )
            .await?;
            anyhow::ensure!(
                saved.len() == 1
                    && saved[0].id == original.id
                    && saved[0].txid == original.txid
                    && saved[0].status == "unconfirmed"
                    && saved[0].block_hash.is_none()
                    && constructed_transactions(fixture.data_dir())? == original_transactions
                    && prepared_payment_journal(fixture.data_dir())? == original_journal,
                "rollback/restart changed the saved payment or signed transactions"
            );
            // Zakura can requeue a prior RPC submission independently of ths.
            // Observe ths's gRPC submissions, not the node's mempool contents.
            anyhow::ensure!(
                proxy.broadcast_counts() == broadcasts_after_reorg,
                "orphaned payment was rebroadcast after rollback/restart"
            );
            let wallet = Connection::open_with_flags(
                fixture.data_dir().join("wallet.db"),
                OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let scanned: u64 =
                wallet.query_row("SELECT MAX(height) FROM blocks", [], |row| row.get(0))?;
            anyhow::ensure!(
                scanned == before.blocks - 3,
                "wallet retained scanned blocks above the canonical tip"
            );
            let denied = client
                .post(format!("{}/api/v1/reorg", fixture.api_url()))
                .timeout(API_READ_TIMEOUT)
                .json(&json!({"depth":1001}))
                .send()
                .await?;
            anyhow::ensure!(
                denied.status() == reqwest::StatusCode::BAD_REQUEST,
                "rollback limit was not rejected"
            );
            let unchanged: ChainInfo =
                rpc(&client, fixture.node_url(), "getblockchaininfo", json!([])).await?;
            anyhow::ensure!(
                unchanged.blocks == after.blocks && unchanged.bestblockhash == after.bestblockhash,
                "rejected depth mutated the chain"
            );
            Ok(())
        }
        .await;
        let cleanup = fixture.shutdown().await;
        let proxy_cleanup = match broadcast_proxy.as_mut() {
            Some(proxy) => proxy.shutdown().await,
            None => Ok(()),
        };
        preserve_scenario_failure(scenario, preserve_scenario_failure(cleanup, proxy_cleanup))?;
    }
    Ok(())
}
