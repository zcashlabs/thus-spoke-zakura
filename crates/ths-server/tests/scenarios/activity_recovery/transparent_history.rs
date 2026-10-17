use std::process::Stdio;

use super::*;

async fn settled_accounts(
    client: &Client,
    base: &str,
    checkpoint: &serde_json::Value,
) -> Result<serde_json::Value> {
    let deadline = Instant::now() + SEND_TIMEOUT;
    loop {
        let status: serde_json::Value =
            request_json(client, base, "/api/v1/status", None, API_READ_TIMEOUT).await?;
        if status["settled"] == true
            && status["wallet_sync_state"] == "ready"
            && status["wallet_sync"]["fully_scanned_hash"] == checkpoint["bestblockhash"]
            && status["wallet_sync"]["observed_hash"] == checkpoint["bestblockhash"]
            && status["wallet_scanned_height"] == checkpoint["blocks"]
        {
            return request_json(client, base, "/api/v1/accounts", None, API_READ_TIMEOUT).await;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "wallet did not settle at the canonical checkpoint: {status}"
        );
        tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
    }
}

pub(super) async fn external_transparent_spend_reconciles_after_restart() -> Result<()> {
    let server = PathBuf::from(env!("CARGO_BIN_EXE_ths-server"));
    let mut fixture = RegtestStack::new(server.clone())?;
    let mut external: Option<tokio::process::Child> = None;
    let second = tempfile::tempdir()?;
    let replacement_wallet = tempfile::tempdir()?;
    let scenario=async {
        fixture.start().await.context("starting primary fixture")?;
        let client=Client::new();
        let funding:Activity=confirmed_payment(&client,fixture.api_url(),"/api/v1/send",Some(&json!({
            "from_account":1,"to_account":2,"source_pool":"ironwood","destination_pool":"transparent",
            "amount_zatoshi":10_000_000,"idempotency_key":"external-transparent-funding"
        })),SEND_TIMEOUT).await.context("funding external-spend source")?;
        let _:Activity=confirmed_payment(&client,fixture.api_url(),"/api/v1/send",Some(&json!({
            "from_account":1,"to_account":4,"source_pool":"ironwood","destination_pool":"transparent",
            "amount_zatoshi":20_000_000,"idempotency_key":"external-transparent-unspent-control"
        })),SEND_TIMEOUT).await.context("funding unspent control")?;
        let tip:serde_json::Value=rpc(&client,fixture.node_url(),"getblockchaininfo",json!([])).await?;
        let before=settled_accounts(&client,fixture.api_url(),&tip).await?;
        anyhow::ensure!(before[1]["transparent_zatoshi"]==10_000_000,"initial transparent funding missing");
        let funding_tx:serde_json::Value=rpc(&client,fixture.node_url(),"getrawtransaction",json!([funding.txid,1])).await?;
        let receiver=before[1]["transparent_address"].as_str().context("missing receiver")?;
        let output=funding_tx["vout"].as_array().context("missing outputs")?.iter().find(|output|output["scriptPubKey"]["addresses"].as_array().is_some_and(|addresses|addresses.iter().any(|address|address==receiver))).context("funding output missing")?["n"].as_u64().context("missing funding index")?;

        // Copy committed development wallet state, then author the new spend only
        // through B. Separate database files prevent A learning it at preparation.
        fixture.interrupt_server().await?;
        for name in ["ths.db","wallet.db"] {
            for target in [second.path(),replacement_wallet.path()] {
                Connection::open(fixture.data_dir().join(name))?.execute("VACUUM INTO ?1",[target.join(name).to_str().context("invalid snapshot path")?])?;
            }
        }
        fixture.restart_server().await.context("restarting primary wallet")?;
        let listener=std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,0))?;
        let port=listener.local_addr()?.port();drop(listener);
        let base=format!("http://127.0.0.1:{port}");
        external=Some(tokio::process::Command::new(&server).args(["serve","--data-dir"]).arg(second.path())
            .env("THS_LISTEN",format!("127.0.0.1:{port}"))
            .env("THS_ZAKURA_RPC",fixture.node_url())
            .env("THS_LIGHTWALLETD",format!("http://127.0.0.1:{}",fixture.lightwalletd_port()?))
            .env("THS_INSTANCE","external-transparent-client")
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn()?);
        let deadline=Instant::now()+SEND_TIMEOUT;
        loop {
            if request_json::<serde_json::Value>(&client,&base,"/api/v1/status",None,API_READ_TIMEOUT).await.is_ok_and(|status|status["settled"]==true) {break;}
            anyhow::ensure!(Instant::now()<deadline,"external wallet did not become ready");
            tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
        }
        let spend:Activity=confirmed_payment(&client,&base,"/api/v1/send",Some(&json!({
            "from_account":2,"to_account":3,"source_pool":"transparent","destination_pool":"transparent",
            "amount_zatoshi":9_990_000,"idempotency_key":"external-transparent-spend"
        })),SEND_TIMEOUT).await.context("confirming external transparent-only spend")?;
        let tx:serde_json::Value=rpc(&client,fixture.node_url(),"getrawtransaction",json!([spend.txid,1])).await?;
        anyhow::ensure!(tx["vin"].as_array().context("missing inputs")?.iter().any(|input|input["txid"]==funding.txid && input["vout"]==output),"external transaction did not consume the funded output");
        let unspent:serde_json::Value=rpc(&client,fixture.node_url(),"gettxout",json!([funding.txid,output,true])).await?;
        anyhow::ensure!(unspent.is_null(),"node still reports original output unspent");
        let tip:serde_json::Value=rpc(&client,fixture.node_url(),"getblockchaininfo",json!([])).await?;
        let a=settled_accounts(&client,fixture.api_url(),&tip).await?;
        let b=settled_accounts(&client,&base,&tip).await?;
        anyhow::ensure!(a[1]["transparent_zatoshi"]==0,"ready wallet retained externally spent transparent output: {}",a[1]["transparent_zatoshi"]);
        for accounts in [&a,&b] {
            anyhow::ensure!(accounts[1]["transparent_zatoshi"]==0 && accounts[1]["ironwood_zatoshi"]==0 && accounts[2]["transparent_zatoshi"]==9_990_000 && accounts[3]["transparent_zatoshi"]==20_000_000,"wallets disagree with canonical debit/change/receipt: {accounts}");
        }
        let _:serde_json::Value=rpc(&client,fixture.node_url(),"generate",json!([1])).await?;
        let later:serde_json::Value=rpc(&client,fixture.node_url(),"getblockchaininfo",json!([])).await?;
        let a=settled_accounts(&client,fixture.api_url(),&later).await?;
        anyhow::ensure!(a[1]["transparent_zatoshi"]==0,"spent output returned after an extra block");
        fixture.restart_server().await.context("restarting primary wallet")?;
        let a=settled_accounts(&client,fixture.api_url(),&later).await?;
        anyhow::ensure!(a[1]["transparent_zatoshi"]==0,"spent output returned after restart");
        let mut child=external.take().context("external wallet missing")?;
        child.kill().await?;
        tokio::time::timeout(Duration::from_secs(10),child.wait()).await??;
        let response=client.post(format!("{}/api/v1/reorg",fixture.api_url())).timeout(SEND_TIMEOUT).json(&json!({"depth":2,"keep_txs":false})).send().await.context("performing same-height reorg")?;
        let status=response.status();
        let reorg:serde_json::Value=response.json().await?;
        if !status.is_success() {
            let sync:serde_json::Value=request_json(&client,fixture.api_url(),"/api/v1/status",None,API_READ_TIMEOUT).await?;
            anyhow::bail!("same-height reorg returned {status}: {reorg}; wallet sync: {}",sync["wallet_sync"]);
        }
        anyhow::ensure!(reorg["old_tip"]["height"]==reorg["new_tip"]["height"],"expected same-height replacement");
        let canonical:serde_json::Value=rpc(&client,fixture.node_url(),"getblockchaininfo",json!([])).await?;
        let restored=settled_accounts(&client,fixture.api_url(),&canonical).await?;
        anyhow::ensure!(restored[1]["transparent_zatoshi"]==10_000_000 && restored[2]["transparent_zatoshi"]==0 && restored[3]["transparent_zatoshi"]==20_000_000,"reorg failed to restore canonical holdings: {restored}");
        let unspent:serde_json::Value=rpc(&client,fixture.node_url(),"gettxout",json!([funding.txid,output,true])).await?;
        anyhow::ensure!(!unspent.is_null(),"reorg did not restore the original output");

        // C has the same funded account but never prepared B's now-orphaned spend.
        // A retains that spend; history must still discover a different spender.
        let listener=std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,0))?;
        let port=listener.local_addr()?.port();drop(listener);
        let base=format!("http://127.0.0.1:{port}");
        external=Some(tokio::process::Command::new(&server).args(["serve","--data-dir"]).arg(replacement_wallet.path())
            .env("THS_LISTEN",format!("127.0.0.1:{port}"))
            .env("THS_ZAKURA_RPC",fixture.node_url())
            .env("THS_LIGHTWALLETD",format!("http://127.0.0.1:{}",fixture.lightwalletd_port()?))
            .env("THS_INSTANCE","external-transparent-replacement")
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn()?);
        let deadline=Instant::now()+SEND_TIMEOUT;
        loop {
            if request_json::<serde_json::Value>(&client,&base,"/api/v1/status",None,API_READ_TIMEOUT).await.is_ok_and(|status|status["settled"]==true) {break;}
            anyhow::ensure!(Instant::now()<deadline,"replacement wallet did not become ready");
            tokio::time::sleep(RECOVERY_POLL_INTERVAL).await;
        }
        let replacement:Activity=confirmed_payment(&client,&base,"/api/v1/send",Some(&json!({
            "from_account":2,"to_account":5,"source_pool":"transparent","destination_pool":"transparent",
            "amount_zatoshi":9_990_000,"idempotency_key":"external-transparent-replacement"
        })),SEND_TIMEOUT).await.context("confirming external replacement spend")?;
        anyhow::ensure!(replacement.txid!=spend.txid,"replacement must be a different transaction");
        let tx:serde_json::Value=rpc(&client,fixture.node_url(),"getrawtransaction",json!([replacement.txid,1])).await?;
        anyhow::ensure!(tx["vin"].as_array().context("missing replacement inputs")?.iter().any(|input|input["txid"]==funding.txid && input["vout"]==output),"replacement did not consume the exact restored output");
        let canonical:serde_json::Value=rpc(&client,fixture.node_url(),"getblockchaininfo",json!([])).await?;
        let after=settled_accounts(&client,fixture.api_url(),&canonical).await?;
        anyhow::ensure!(after[1]["transparent_zatoshi"]==0 && after[2]["transparent_zatoshi"]==0 && after[3]["transparent_zatoshi"]==20_000_000 && after[4]["transparent_zatoshi"]==9_990_000,"replacement spend was not reconciled: {after}");
        eprintln!("Confirmed external transparent-only spend, unspent control, restart, same-height reorg and distinct replacement spend at height {}",canonical["blocks"]);
        Ok(())
    }.await;
    let external_cleanup = async {
        if let Some(child) = external.as_mut() {
            child.kill().await?;
            tokio::time::timeout(Duration::from_secs(10), child.wait()).await??;
        }
        Ok(())
    }
    .await;
    let cleanup = fixture.shutdown().await;
    preserve_scenario_failure(scenario, external_cleanup)?;
    preserve_scenario_failure(Ok(()), cleanup)
}
