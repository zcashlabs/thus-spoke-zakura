use crate::{
    intents::IntentJournal,
    runtime::{Endpoints, InstanceName, Runtime, container_running, prefix},
};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct MiningJob {
    pub(crate) id: String,
    pub(crate) requested_blocks: u32,
    pub(crate) completed_blocks: u32,
    pub(crate) state: MiningJobState,
    pub(crate) error: Option<String>,
    pub(crate) progress_uncertain: bool,
    // Older servers may omit this; treat it the same as "not completed yet".
    #[serde(default)]
    pub(crate) hashes: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MiningJobState {
    Mining,
    Syncing,
    Completed,
    Failed,
}

/// `--json` output for `mine`. Keeps the `blocks`/`hashes` fields and meaning
/// from the legacy endpoint's successful output, alongside the job API's
/// richer state so a partial or failed job is not just a bare error string.
#[derive(Debug, Serialize)]
struct MineOutput {
    id: String,
    blocks: u32,
    requested_blocks: u32,
    state: MiningJobState,
    error: Option<String>,
    progress_uncertain: bool,
    hashes: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FaucetResult {
    address: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    #[serde(default = "confirmed_status")]
    status: String,
    #[serde(default)]
    last_error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ChainTip {
    height: u64,
    hash: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ChainChangeResult {
    depth: u32,
    old_tip: ChainTip,
    new_tip: ChainTip,
    removed_hashes: Vec<String>,
    replacement_hashes: Vec<String>,
    retained_txids: Vec<String>,
    failed_transactions: Vec<TransactionFailure>,
}

#[derive(Debug, Deserialize, Serialize)]
struct TransactionFailure {
    txid: String,
    error: String,
}

pub(crate) fn change_chain(
    dashboard: &str,
    client: &reqwest::blocking::Client,
    operation: &str,
    depth: u32,
    keep_txs: bool,
) -> Result<ChainChangeResult> {
    let body = if operation == "reorg" {
        serde_json::json!({"depth": depth, "keep_txs": keep_txs})
    } else {
        serde_json::json!({"depth": depth})
    };
    client
        .post(format!("{dashboard}/api/v1/{operation}"))
        .json(&body)
        .send()
        .map_err(anyhow::Error::from)
        .and_then(decode_response)
        .context("chain operation failed; inspect the current tip before retrying")
}

pub(crate) fn render_chain_change(
    result: &ChainChangeResult,
    name: &InstanceName,
    operation: &str,
    json: bool,
) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(result)?);
    }
    let action = if operation == "reorg" {
        "Reorganized"
    } else {
        "Rolled back"
    };
    let mut output = format!(
        "{action} {} blocks on {name}.\nNew tip: {} ({})",
        result.depth, result.new_tip.height, result.new_tip.hash
    );
    for failure in &result.failed_transactions {
        output.push_str(&format!(
            "\nNot retained: {}: {}",
            failure.txid, failure.error
        ));
    }
    Ok(output)
}

fn confirmed_status() -> String {
    "confirmed".to_owned()
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Activity {
    id: String,
    kind: String,
    from_account: Option<u8>,
    to_account: Option<u8>,
    to_address: Option<String>,
    source_pool: String,
    destination_pool: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    status: String,
    last_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ActivityHistoryEntry {
    kind: String,
    from_account: Option<u8>,
    to_account: Option<u8>,
    #[serde(default)]
    to_address: Option<String>,
    source_pool: String,
    destination_pool: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    status: String,
    created_at: String,
    #[serde(default)]
    last_error: Option<String>,
}

/// How long one payment command waits for its accepted payments to settle. The environment keeps
/// processing them after the command stops waiting.
const PAYMENT_WAIT: Duration = Duration::from_secs(30);
const PAYMENT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long `mine` waits for its job to finish. The environment keeps mining
/// after the command stops waiting, the same as a payment.
const MINE_WAIT: Duration = Duration::from_secs(300);
const MINE_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Caps one request (submitting or polling a job), matching the single-HTTP-
/// attempt budget used for readiness probes elsewhere. A stalled or dropped
/// connection errors out on its own instead of blocking the overall deadline.
const MINE_POLL_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

trait Payment: serde::de::DeserializeOwned + Serialize {
    fn status(&self) -> &str;

    fn settled(&self) -> bool {
        matches!(self.status(), "confirmed" | "failed")
    }
}

impl Payment for Activity {
    fn status(&self) -> &str {
        &self.status
    }
}

impl Payment for FaucetResult {
    fn status(&self) -> &str {
        &self.status
    }
}

/// What one invocation observed about a payment it followed by its retained key.
enum Followed<T> {
    /// `confirmed` or `failed`.
    Settled(T),
    Pending(T),
    /// The environment refused the request (4xx), so it was never accepted.
    Rejected(anyhow::Error),
    /// Neither the submission nor any status read returned the operation; it may still exist.
    Unknown(anyhow::Error),
}

fn remaining(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
}

/// Submits each key once, then reads the status of unsettled ones about once per second until
/// `deadline`. Status reads never resubmit, and every network wait is bounded by the time left.
fn follow_payments<T: Payment>(
    deadline: Instant,
    keys: &[String],
    json: bool,
    mut submit: impl FnMut(&str, Duration) -> reqwest::Result<reqwest::blocking::Response>,
    mut status: impl FnMut(&str, Duration) -> Result<Option<T>>,
) -> Vec<Followed<T>> {
    let mut followed: Vec<_> = keys
        .iter()
        .map(|key| {
            let Some(left) = remaining(deadline) else {
                return Followed::Unknown(anyhow!(
                    "the wait ended before this payment was submitted"
                ));
            };
            match submit(key, left) {
                Ok(response) => {
                    let refused = response.status().is_client_error();
                    match decode_response::<T>(response) {
                        Ok(payment) if payment.settled() => Followed::Settled(payment),
                        Ok(payment) => Followed::Pending(payment),
                        Err(error) if refused => Followed::Rejected(error),
                        Err(error) => Followed::Unknown(error),
                    }
                }
                Err(error) => Followed::Unknown(error.into()),
            }
        })
        .collect();
    let unsettled =
        |entry: &Followed<T>| matches!(entry, Followed::Pending(_) | Followed::Unknown(_));
    // A round runs only when its status reads still have time after the full sleep.
    let round_time = || remaining(deadline).filter(|left| *left > PAYMENT_POLL_INTERVAL);
    if !json
        && followed.iter().any(unsettled)
        && let Some(left) = round_time()
    {
        eprintln!(
            "Waiting up to {} seconds for confirmation. Ctrl+C stops waiting; the environment keeps processing the payment.",
            left.as_secs()
        );
    }
    while followed.iter().any(unsettled) && round_time().is_some() {
        std::thread::sleep(PAYMENT_POLL_INTERVAL);
        for (key, entry) in keys.iter().zip(&mut followed) {
            if !unsettled(entry) {
                continue;
            }
            let Some(left) = remaining(deadline) else {
                break;
            };
            match status(key, left) {
                Ok(Some(payment)) if payment.settled() => *entry = Followed::Settled(payment),
                Ok(Some(payment)) => *entry = Followed::Pending(payment),
                // A pending payment keeps its last observed state; a failed read is not a failure.
                Ok(None) => {
                    if let Followed::Unknown(_) = entry {
                        *entry = Followed::Unknown(anyhow!(
                            "the environment has no record of this payment yet"
                        ));
                    }
                }
                Err(error) => {
                    if let Followed::Unknown(_) = entry {
                        *entry = Followed::Unknown(error);
                    }
                }
            }
        }
    }
    followed
}

/// Reads an account payment by its idempotency key without resubmitting it.
fn activity_status(
    client: &reqwest::blocking::Client,
    dashboard: &str,
    key: &str,
    timeout: Duration,
) -> Result<Option<Activity>> {
    let mut matches: Vec<Activity> = client
        .get(format!("{dashboard}/api/v1/activity"))
        .query(&[("idempotency_key", key)])
        .timeout(timeout)
        .send()
        .map_err(anyhow::Error::from)
        .and_then(decode_response)
        .context("reading payment status")?;
    if matches.len() > 1 {
        bail!(
            "reading payment status: the environment returned {} activities for one idempotency key",
            matches.len()
        );
    }
    Ok(matches.pop())
}

/// Reads an address faucet payment by its idempotency key without resubmitting it.
fn address_faucet_status(
    client: &reqwest::blocking::Client,
    dashboard: &str,
    key: &str,
    timeout: Duration,
) -> Result<Option<FaucetResult>> {
    let response = client
        .get(format!("{dashboard}/api/v1/faucet/address"))
        .query(&[("idempotency_key", key)])
        .timeout(timeout)
        .send()
        .context("reading payment status")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    decode_response(response)
        .map(Some)
        .context("reading payment status")
}

/// Operation JSON for a payment this invocation could not settle, plus the key that resumes it.
pub(crate) fn unresolved_json<T: Serialize>(
    payment: Option<&T>,
    key: &str,
) -> Result<serde_json::Value> {
    let mut value = match payment {
        Some(payment) => serde_json::to_value(payment)?,
        None => serde_json::json!({"status": "unknown"}),
    };
    value["idempotency_key"] = key.into();
    Ok(value)
}

impl Activity {
    fn recipient(&self) -> String {
        recipient(self.to_account, self.to_address.as_deref())
    }
}

fn recipient(to_account: Option<u8>, to_address: Option<&str>) -> String {
    match (to_account, to_address) {
        (Some(id), _) => format!("account {id}"),
        (None, Some(address)) => address.to_owned(),
        (None, None) => "an unknown recipient".to_owned(),
    }
}

/// A single-payment ZIP-321 URI as resolved by the server.
#[derive(Deserialize)]
struct PaymentUri {
    address: String,
    destination_pool: String,
    amount_zatoshi: Option<u64>,
    memo: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SendQuote {
    max_zatoshi: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransferAmount {
    Exact(u64),
    Max,
}

fn quote_max_amount(
    client: &reqwest::blocking::Client,
    dashboard: &str,
    name: &InstanceName,
    deadline: Instant,
    from: u8,
    source_pool: &str,
    destination_pool: &str,
) -> Result<u64> {
    let timeout = remaining(deadline)
        .ok_or_else(|| anyhow!("maximum transfer quote timed out"))
        .with_context(|| format!("asking environment {name} for the maximum transfer amount"))?;
    let quote: SendQuote = client
        .post(format!("{dashboard}/api/v1/send/quote"))
        .json(&serde_json::json!({
            "from_account": from,
            "source_pool": source_pool,
            "destination_pool": destination_pool,
        }))
        .timeout(timeout)
        .send()
        .map_err(anyhow::Error::from)
        .and_then(decode_response)
        .with_context(|| format!("asking environment {name} for the maximum transfer amount"))?;
    if quote.max_zatoshi == 0 {
        bail!(
            "account {from} has no spendable {source_pool} funds remaining after the network fee"
        );
    }
    Ok(quote.max_zatoshi)
}

impl Runtime {
    fn instance_client(&self, name: &InstanceName) -> Result<(String, reqwest::blocking::Client)> {
        let app_container = format!("{}-app", prefix(name));
        if !container_running(&app_container)? {
            bail!("environment {name} is not running; start it with `ths --name {name}`");
        }
        let dashboard = self.read_instance(name)?.endpoints.dashboard;
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        Ok((dashboard, client))
    }

    pub fn mine(&self, name: &InstanceName, blocks: u32, json: bool) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        self.mine_with(
            name,
            &dashboard,
            &client,
            std::io::stdout().lock(),
            Instant::now() + MINE_WAIT,
            blocks,
            json,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn mine_with(
        &self,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        mut out: impl std::io::Write,
        deadline: Instant,
        blocks: u32,
        json: bool,
    ) -> Result<()> {
        let journal = IntentJournal::open(&self.instance_dir(name), "mining")?;
        let intent = format!("mine:{blocks}");
        let idempotency_key = journal.key_for(&intent)?;

        let mut job = submit_mining_job(client, dashboard, blocks, &idempotency_key, deadline)
            .with_context(|| format!("asking environment {name} to mine {blocks} blocks"))?;

        let mut last_printed = None;
        let timed_out = loop {
            if !json {
                let line = mine_status_line(&job, name);
                if last_printed.as_ref() != Some(&line) {
                    writeln!(out, "{line}")?;
                    last_printed = Some(line);
                }
            }
            match job.state {
                MiningJobState::Completed | MiningJobState::Failed => break false,
                MiningJobState::Mining | MiningJobState::Syncing => {}
            }
            let Some(left) = remaining(deadline) else {
                break true;
            };
            std::thread::sleep(MINE_POLL_INTERVAL.min(left));
            // A poll's own failure (a stall, a reset, a dropped connection,
            // anything that is not an actual response from the server) says
            // nothing about whether the job is still running, so it is
            // retried within the deadline rather than failing the command.
            // Only a real response, even an error one, is authoritative and
            // handled by `decode_response`.
            let Some(left) = remaining(deadline) else {
                break true;
            };
            match client
                .get(format!("{dashboard}/api/v1/mining/jobs/{}", job.id))
                .timeout(MINE_POLL_ATTEMPT_TIMEOUT.min(left))
                .send()
            {
                Ok(response) => {
                    job = decode_response(response)
                        .with_context(|| format!("checking mining job {} on {name}", job.id))?;
                }
                Err(_) => continue,
            }
        };

        if timed_out {
            if json {
                let output = unresolved_json(Some(&job), &idempotency_key)?;
                writeln!(out, "{}", serde_json::to_string_pretty(&output)?)?;
            } else {
                writeln!(out, "{}", mine_status_line(&job, name))?;
                writeln!(out, "Run the same command to check it again.")?;
            }
            bail!(
                "mining job {} on {name} is still running; run the same command to check it again",
                job.id
            );
        }

        if json {
            let output = MineOutput {
                id: job.id.clone(),
                blocks: job.completed_blocks,
                requested_blocks: job.requested_blocks,
                state: job.state,
                error: job.error.clone(),
                progress_uncertain: job.progress_uncertain,
                hashes: job.hashes.clone(),
            };
            writeln!(out, "{}", serde_json::to_string_pretty(&output)?)?;
        }

        // The job reached a terminal state: replaying this key would only ever
        // return this same frozen job, so forget it and let a future `mine`
        // with the same block count start a fresh one. Cleared only after the
        // output above is written: if that write fails, the key must still
        // survive so a retry does not start a second job for output we never
        // actually delivered.
        journal.clear(&[(&intent, &idempotency_key)])?;

        if job.state == MiningJobState::Failed {
            bail!(job.error.unwrap_or_else(|| "mining failed".to_owned()));
        }
        Ok(())
    }

    pub(crate) fn change_chain(
        &self,
        name: &InstanceName,
        operation: &str,
        depth: u32,
        keep_txs: bool,
        json: bool,
    ) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        let result = change_chain(&dashboard, &client, operation, depth, keep_txs)?;
        println!("{}", render_chain_change(&result, name, operation, json)?);
        if !result.failed_transactions.is_empty() {
            bail!(
                "chain changed, but {} transaction(s) could not be retained",
                result.failed_transactions.len()
            );
        }
        Ok(())
    }

    pub fn faucet(
        &self,
        name: &InstanceName,
        address: &str,
        amount_zatoshi: u64,
        json: bool,
    ) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        self.faucet_with(
            name,
            &dashboard,
            &client,
            std::io::stdout().lock(),
            Instant::now() + PAYMENT_WAIT,
            address,
            amount_zatoshi,
            json,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn faucet_with(
        &self,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        mut out: impl std::io::Write,
        deadline: Instant,
        address: &str,
        amount_zatoshi: u64,
        json: bool,
    ) -> Result<()> {
        let journal = IntentJournal::open(&self.instance_dir(name), "faucet")?;
        let intent = format!("address:{address}:{amount_zatoshi}");
        let idempotency_key = journal.key_for(&intent)?;
        let followed = follow_payments(
            deadline,
            std::slice::from_ref(&idempotency_key),
            json,
            |key, timeout| {
                client
                    .post(format!("{dashboard}/api/v1/faucet/address"))
                    .json(&serde_json::json!({
                        "address": address,
                        "amount_zatoshi": amount_zatoshi,
                        "idempotency_key": key,
                    }))
                    .timeout(timeout)
                    .send()
            },
            |key, timeout| address_faucet_status(client, dashboard, key, timeout),
        )
        .remove(0);
        let result = match followed {
            Followed::Settled(result) => result,
            Followed::Pending(result) => {
                if json {
                    let output = unresolved_json(Some(&result), &idempotency_key)?;
                    writeln!(out, "{}", serde_json::to_string_pretty(&output)?)?;
                } else {
                    if !result.txid.is_empty() {
                        writeln!(out, "Transaction: {}", result.txid)?;
                    }
                    writeln!(
                        out,
                        "Payment to {} is pending ({}). Idempotency key: {idempotency_key}",
                        result.address, result.status
                    )?;
                    writeln!(out, "Run the same command to check it again.")?;
                }
                bail!("faucet payment is pending; run the same command to check it again")
            }
            Followed::Unknown(error) => {
                if json {
                    let output = unresolved_json::<FaucetResult>(None, &idempotency_key)?;
                    writeln!(out, "{}", serde_json::to_string_pretty(&output)?)?;
                } else {
                    writeln!(
                        out,
                        "Payment to {address} is unknown. Idempotency key: {idempotency_key}"
                    )?;
                    writeln!(out, "Run the same command to check it again.")?;
                }
                return Err(error.context(format!(
                    "could not read the faucet payment status from environment {name}; run the same command to check it again"
                )));
            }
            Followed::Rejected(error) => {
                return Err(error.context(format!("asking environment {name} to fund {address}")));
            }
        };
        if result.status == "confirmed" && result.block_hash.is_none() {
            bail!("faucet reported confirmation without a block hash; retry the same command");
        }
        journal.clear(&[(&intent, &idempotency_key)])?;
        if json {
            writeln!(out, "{}", serde_json::to_string_pretty(&result)?)?;
        }
        if result.status == "failed" {
            bail!(
                "faucet payment to {} failed: {}",
                result.address,
                result.last_error.as_deref().unwrap_or("no reason recorded")
            );
        }
        if !json {
            writeln!(out, "Transaction: {}", result.txid)?;
            writeln!(
                out,
                "Sent {} ZEC to {} on {name}.",
                format_zec(result.amount_zatoshi),
                result.address
            )?;
            if let Some(block_hash) = &result.block_hash {
                writeln!(out, "Confirmed in: {block_hash}")?;
            }
        }
        Ok(())
    }

    pub fn wallet_faucet(
        &self,
        name: &InstanceName,
        accounts: &[u8],
        amount_zatoshi: u64,
        pool: &str,
        json: bool,
    ) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        self.wallet_faucet_with(
            name,
            &dashboard,
            &client,
            std::io::stdout().lock(),
            Instant::now() + PAYMENT_WAIT,
            accounts,
            amount_zatoshi,
            pool,
            json,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn wallet_faucet_with(
        &self,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        mut out: impl std::io::Write,
        deadline: Instant,
        accounts: &[u8],
        amount_zatoshi: u64,
        pool: &str,
        json: bool,
    ) -> Result<()> {
        let journal = IntentJournal::open(&self.instance_dir(name), "faucet")?;
        let batch = journal.wallet_batch(pool, amount_zatoshi, accounts)?;
        let intents: Vec<_> = accounts.iter().map(|id| format!("{batch}:{id}")).collect();
        // Every component keeps its key until the whole batch settles, so a resumed batch replays
        // completed components by key instead of paying them again.
        let keys = intents
            .iter()
            .map(|intent| journal.key_for(intent))
            .collect::<Result<Vec<_>>>()?;
        let account_for = |key: &str| {
            keys.iter()
                .position(|candidate| candidate == key)
                .map(|index| accounts[index])
                .expect("followed keys belong to this batch")
        };
        let followed = follow_payments(
            deadline,
            &keys,
            json,
            |key, timeout| {
                client
                    .post(format!("{dashboard}/api/v1/faucet"))
                    .json(&serde_json::json!({
                        "account_id": account_for(key),
                        "pool": pool,
                        "amount_zatoshi": amount_zatoshi,
                        "idempotency_key": key,
                    }))
                    .timeout(timeout)
                    .send()
            },
            |key, timeout| activity_status(client, dashboard, key, timeout),
        );
        let mut funded = Vec::new();
        let mut pending = Vec::new();
        let mut failures = Vec::new();
        for ((&account_id, key), outcome) in accounts.iter().zip(&keys).zip(&followed) {
            match outcome {
                Followed::Settled(activity) if activity.status == "confirmed" => {
                    funded.push(activity)
                }
                Followed::Settled(activity) => failures.push(format!(
                    "account {account_id}: payment failed: {}",
                    activity
                        .last_error
                        .as_deref()
                        .unwrap_or("no reason recorded")
                )),
                Followed::Pending(activity) => {
                    pending.push((account_id, unresolved_json(Some(activity), key)?))
                }
                Followed::Unknown(error) => {
                    let mut unknown = unresolved_json::<Activity>(None, key)?;
                    unknown["to_account"] = account_id.into();
                    unknown["error"] = format!("{error:#}").into();
                    pending.push((account_id, unknown));
                }
                Followed::Rejected(error) => {
                    failures.push(format!("account {account_id}: {error:#}"))
                }
            }
        }
        if followed
            .iter()
            .all(|outcome| matches!(outcome, Followed::Settled(_)))
        {
            let settled: Vec<_> = intents
                .iter()
                .zip(&keys)
                .map(|(intent, key)| (intent.as_str(), key.as_str()))
                .collect();
            journal.clear(&settled)?;
        }
        if json {
            let pending: Vec<_> = pending.iter().map(|(_, value)| value).collect();
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"funded": funded, "pending": pending, "failed": failures})
                )?
            )?;
        } else {
            for activity in &funded {
                writeln!(
                    out,
                    "Funded {} with {} ZEC ({} pool) on {name}.",
                    activity.recipient(),
                    format_zec(activity.amount_zatoshi),
                    activity.destination_pool
                )?;
                writeln!(out, "  Transaction: {}", activity.txid)?;
            }
            for (account_id, unresolved) in &pending {
                eprintln!(
                    "Payment to account {account_id} is {} (idempotency key {}). Run the same command to check it again.",
                    unresolved["status"].as_str().unwrap_or("pending"),
                    unresolved["idempotency_key"].as_str().unwrap_or_default()
                );
            }
            for failure in &failures {
                eprintln!("Failed to fund {failure}");
            }
        }
        if failures.is_empty() && pending.is_empty() {
            Ok(())
        } else {
            bail!(
                "{} of {} faucet requests are pending or failed",
                failures.len() + pending.len(),
                accounts.len()
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn wallet_send(
        &self,
        name: &InstanceName,
        from: u8,
        to: u8,
        source_pool: &str,
        destination_pool: &str,
        amount: TransferAmount,
        memo: Option<&str>,
        json: bool,
    ) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        self.wallet_send_with(
            name,
            &dashboard,
            &client,
            std::io::stdout().lock(),
            Instant::now() + PAYMENT_WAIT,
            from,
            to,
            source_pool,
            destination_pool,
            amount,
            memo,
            json,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn wallet_send_with(
        &self,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        out: impl std::io::Write,
        deadline: Instant,
        from: u8,
        to: u8,
        source_pool: &str,
        destination_pool: &str,
        amount: TransferAmount,
        memo: Option<&str>,
        json: bool,
    ) -> Result<()> {
        self.submit_transfer(
            name,
            dashboard,
            client,
            out,
            deadline,
            from,
            amount,
            serde_json::json!({
                "from_account": from,
                "to_account": to,
                "source_pool": source_pool,
                "destination_pool": destination_pool,
                "memo": memo,
            }),
            source_pool,
            destination_pool,
            memo,
            json,
        )
    }

    /// Pays a ZIP-321 URI. The server parses it, so the CLI and the dashboard
    /// accept exactly the same URIs.
    pub fn wallet_send_uri(
        &self,
        name: &InstanceName,
        from: u8,
        source_pool: &str,
        uri: &str,
        amount: Option<TransferAmount>,
        json: bool,
    ) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        self.wallet_send_uri_with(
            name,
            &dashboard,
            &client,
            from,
            source_pool,
            uri,
            amount,
            json,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn wallet_send_uri_with(
        &self,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        from: u8,
        source_pool: &str,
        uri: &str,
        amount: Option<TransferAmount>,
        json: bool,
    ) -> Result<()> {
        let response = client
            .post(format!("{dashboard}/api/v1/zip321/parse"))
            .json(&serde_json::json!({ "uri": uri }))
            .send()
            .with_context(|| format!("asking environment {name} to read the payment URI"))?;
        let request: PaymentUri = decode_response(response)?;
        let amount = match (request.amount_zatoshi, amount) {
            (Some(amount), None) => TransferAmount::Exact(amount),
            (None, Some(amount)) => amount,
            (Some(_), Some(TransferAmount::Exact(_))) => {
                bail!("the payment URI already sets an amount; omit --amount")
            }
            (Some(_), Some(TransferAmount::Max)) => {
                bail!("the payment URI already sets an amount; omit --max")
            }
            (None, None) => bail!("the payment URI has no amount; pass --amount or --max"),
        };
        self.submit_transfer(
            name,
            dashboard,
            client,
            std::io::stdout().lock(),
            Instant::now() + PAYMENT_WAIT,
            from,
            amount,
            serde_json::json!({
                "from_account": from,
                "to_address": request.address,
                "source_pool": source_pool,
                "destination_pool": request.destination_pool,
                "memo": request.memo,
            }),
            source_pool,
            &request.destination_pool,
            request.memo.as_deref(),
            json,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_transfer(
        &self,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        out: impl std::io::Write,
        deadline: Instant,
        from: u8,
        amount: TransferAmount,
        mut body: serde_json::Value,
        source_pool: &str,
        destination_pool: &str,
        memo: Option<&str>,
        json: bool,
    ) -> Result<()> {
        match amount {
            TransferAmount::Exact(amount) => {
                body["amount_zatoshi"] = amount.into();
                self.submit_send(
                    name, dashboard, client, out, deadline, from, body, memo, json,
                )
            }
            TransferAmount::Max => self.submit_max_send(
                name,
                dashboard,
                client,
                out,
                deadline,
                from,
                body,
                source_pool,
                destination_pool,
                memo,
                json,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_max_send(
        &self,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        out: impl std::io::Write,
        deadline: Instant,
        from: u8,
        mut body: serde_json::Value,
        source_pool: &str,
        destination_pool: &str,
        memo: Option<&str>,
        json: bool,
    ) -> Result<()> {
        let journal = IntentJournal::open(&self.instance_dir(name), "send")?;
        let entries = journal.read()?;
        let saved = entries.keys().find_map(|fingerprint| {
            let value: serde_json::Value = serde_json::from_str(fingerprint).ok()?;
            (value.get("amount_mode")?.as_str()? == "max" && value.get("request")? == &body).then(
                || {
                    value
                        .get("amount_zatoshi")?
                        .as_u64()
                        .map(|amount| (fingerprint.clone(), amount))
                },
            )?
        });
        let (fingerprint, amount_zatoshi) = match saved {
            Some(saved) => saved,
            None => {
                let amount = quote_max_amount(
                    client,
                    dashboard,
                    name,
                    deadline,
                    from,
                    source_pool,
                    destination_pool,
                )?;
                let fingerprint = serde_json::to_string(&serde_json::json!({
                    "amount_mode": "max",
                    "amount_zatoshi": amount,
                    "request": body.clone(),
                }))?;
                (fingerprint, amount)
            }
        };
        body["amount_zatoshi"] = amount_zatoshi.into();
        self.submit_send_with_journal(
            &journal,
            &fingerprint,
            name,
            dashboard,
            client,
            out,
            deadline,
            from,
            body,
            memo,
            json,
        )
    }

    /// Submits `body` under the idempotency key saved for it, so running the same command again
    /// checks the original payment instead of creating another.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn submit_send(
        &self,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        out: impl std::io::Write,
        deadline: Instant,
        from: u8,
        body: serde_json::Value,
        memo: Option<&str>,
        json: bool,
    ) -> Result<()> {
        let journal = IntentJournal::open(&self.instance_dir(name), "send")?;
        // serde_json sorts object keys, so the body before its key is added identifies the request
        // exactly: every field distinguishes intents, and an absent memo (`null`) differs from an
        // empty one.
        let fingerprint = serde_json::to_string(&body)?;
        self.submit_send_with_journal(
            &journal,
            &fingerprint,
            name,
            dashboard,
            client,
            out,
            deadline,
            from,
            body,
            memo,
            json,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_send_with_journal(
        &self,
        journal: &IntentJournal,
        fingerprint: &str,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        mut out: impl std::io::Write,
        deadline: Instant,
        from: u8,
        mut body: serde_json::Value,
        memo: Option<&str>,
        json: bool,
    ) -> Result<()> {
        let resumed = journal.read()?.contains_key(fingerprint);
        let idempotency_key = journal.key_for(fingerprint)?;
        if resumed && !json {
            eprintln!("Checking the saved payment with idempotency key {idempotency_key}.");
        }
        body["idempotency_key"] = idempotency_key.as_str().into();
        let followed = follow_payments(
            deadline,
            std::slice::from_ref(&idempotency_key),
            json,
            |_, timeout| {
                client
                    .post(format!("{dashboard}/api/v1/send"))
                    .json(&body)
                    .timeout(timeout)
                    .send()
            },
            |key, timeout| activity_status(client, dashboard, key, timeout),
        )
        .remove(0);
        let activity = match followed {
            Followed::Settled(activity) => activity,
            Followed::Pending(activity) => {
                if json {
                    let output = unresolved_json(Some(&activity), &idempotency_key)?;
                    writeln!(out, "{}", serde_json::to_string_pretty(&output)?)?;
                } else {
                    if !activity.txid.is_empty() {
                        writeln!(out, "Transaction: {}", activity.txid)?;
                    }
                    writeln!(
                        out,
                        "Payment from account {from} to {} is pending ({}). Idempotency key: {idempotency_key}",
                        activity.recipient(),
                        activity.status
                    )?;
                    writeln!(out, "Run the same command to check it again.")?;
                }
                bail!("payment is pending; run the same command to check it again")
            }
            Followed::Unknown(error) => {
                if json {
                    let output = unresolved_json::<Activity>(None, &idempotency_key)?;
                    writeln!(out, "{}", serde_json::to_string_pretty(&output)?)?;
                } else {
                    writeln!(
                        out,
                        "Payment from account {from} is unknown. Idempotency key: {idempotency_key}"
                    )?;
                    writeln!(out, "Run the same command to check it again.")?;
                }
                return Err(error.context(format!(
                    "could not read the payment status from environment {name}; run the same command to check it again"
                )));
            }
            Followed::Rejected(error) => {
                return Err(error.context(format!("asking environment {name} to send")));
            }
        };
        journal.clear(&[(fingerprint, &idempotency_key)])?;
        if json {
            writeln!(out, "{}", serde_json::to_string_pretty(&activity)?)?;
        }
        if activity.status == "failed" {
            bail!(
                "payment from account {from} to {} failed: {}",
                activity.recipient(),
                activity
                    .last_error
                    .as_deref()
                    .unwrap_or("no reason recorded")
            );
        }
        if !json {
            writeln!(
                out,
                "Sent {} ZEC from account {} ({} pool) to {} ({} pool) on {name}.",
                format_zec(activity.amount_zatoshi),
                from,
                activity.source_pool,
                activity.recipient(),
                activity.destination_pool
            )?;
            if let Some(memo) = memo {
                writeln!(out, "Memo: {memo}")?;
            }
            writeln!(out, "Transaction: {}", activity.txid)?;
            if let Some(block_hash) = &activity.block_hash {
                writeln!(out, "Confirmed in: {block_hash}")?;
            }
        }
        Ok(())
    }
}

pub(crate) fn decode_response<T: serde::de::DeserializeOwned>(
    response: reqwest::blocking::Response,
) -> Result<T> {
    let status = response.status();
    if !status.is_success() {
        let detail = response
            .text()
            .unwrap_or_else(|_| "response body was unreadable".to_owned());
        bail!("rejected ({status}): {detail}");
    }
    response.json().context("decoding response")
}

/// Only retries a transport-level send failure, where it is unknown whether
/// the server received the request. A decoded HTTP response, even an error
/// one, confirms the server saw this attempt, so it is returned immediately.
/// Bounded by the same overall deadline as polling: a lost response after the
/// server already accepted the request must not submit a second mining
/// operation with a different idempotency key.
fn submit_mining_job(
    client: &reqwest::blocking::Client,
    dashboard: &str,
    blocks: u32,
    idempotency_key: &str,
    deadline: Instant,
) -> Result<MiningJob> {
    let body = serde_json::json!({"blocks": blocks, "idempotency_key": idempotency_key});
    loop {
        let Some(left) = remaining(deadline) else {
            bail!(
                "timed out submitting the mining job; it may have been accepted and could \
                 still be running. Run the same command to check it again."
            );
        };
        if let Ok(response) = client
            .post(format!("{dashboard}/api/v1/mining/jobs"))
            .json(&body)
            .timeout(MINE_POLL_ATTEMPT_TIMEOUT.min(left))
            .send()
        {
            return decode_response(response);
        }
        // An unreachable server fails instantly, so without a pause this
        // would otherwise spin for the whole deadline instead of retrying
        // at a reasonable rate.
        std::thread::sleep(MINE_POLL_INTERVAL.min(left));
    }
}

/// Wording mirrors the dashboard's `MiningStatus` component, so the CLI and
/// the dashboard describe the same job the same way. The tip hash on
/// completion and the job ID on failure restore what the legacy endpoint's
/// text output showed, since the job itself is otherwise a bare count.
pub(crate) fn mine_status_line(job: &MiningJob, name: &InstanceName) -> String {
    let count = format!(
        "{} of {} blocks confirmed on {name}",
        job.completed_blocks, job.requested_blocks
    );
    let progress = if job.progress_uncertain {
        format!("At least {count}; the last mining result is uncertain.")
    } else {
        format!("{count}.")
    };
    let state = match job.state {
        MiningJobState::Mining => "Mining blocks…".to_owned(),
        MiningJobState::Syncing => "Synchronizing wallet…".to_owned(),
        MiningJobState::Completed => match job.hashes.as_ref().and_then(|hashes| hashes.last()) {
            Some(tip) => format!("Mining completed. Wallet synchronized. New tip: {tip}"),
            None => "Mining completed. Wallet synchronized.".to_owned(),
        },
        MiningJobState::Failed => format!("Mining job {} failed.", job.id),
    };
    format!("{progress} {state}")
}

fn format_zec(zatoshi: u64) -> String {
    let whole = zatoshi / 100_000_000;
    let fraction = zatoshi % 100_000_000;
    if fraction == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{fraction:08}")
            .trim_end_matches('0')
            .to_owned()
    }
}

pub(crate) fn activity_history(
    dashboard: &str,
    client: &reqwest::blocking::Client,
    name: &InstanceName,
    limit: Option<u32>,
) -> Result<Vec<serde_json::Value>> {
    let mut request = client.get(format!("{dashboard}/api/v1/activity"));
    if let Some(limit) = limit {
        request = request.query(&[("limit", limit)]);
    }
    request
        .send()
        .map_err(anyhow::Error::from)
        .and_then(decode_response)
        .with_context(|| format!("asking environment {name} for wallet activity"))
}

fn activity_kind(activity: &ActivityHistoryEntry) -> &'static str {
    if activity.kind == "faucet" {
        "faucet"
    } else {
        match (
            activity.source_pool.as_str(),
            activity.destination_pool.as_str(),
        ) {
            ("transparent", "ironwood") => "shield",
            ("ironwood", "transparent") => "unshield",
            _ => "send",
        }
    }
}

pub(crate) fn activity_lines(activity: &[serde_json::Value]) -> Result<String> {
    if activity.is_empty() {
        return Ok("No wallet activity yet.".to_owned());
    }

    activity
        .iter()
        .map(|value| {
            let entry: ActivityHistoryEntry =
                serde_json::from_value(value.clone()).context("decoding wallet activity entry")?;
            let source = if entry.kind == "faucet" {
                format!("faucet ({})", entry.source_pool)
            } else {
                match entry.from_account {
                    Some(account) => format!("account {account} ({})", entry.source_pool),
                    None => format!("unknown account ({})", entry.source_pool),
                }
            };
            let destination = recipient(entry.to_account, entry.to_address.as_deref());
            let mut lines = vec![
                format!(
                    "{}  {}  {} ZEC",
                    activity_kind(&entry),
                    entry.status,
                    format_zec(entry.amount_zatoshi)
                ),
                format!(
                    "  Route: {source} -> {destination} ({})",
                    entry.destination_pool
                ),
            ];
            if !entry.txid.trim().is_empty() {
                lines.push(format!("  Transaction: {}", entry.txid));
            }
            if let Some(block_hash) = &entry.block_hash {
                lines.push(format!("  Confirmed in: {block_hash}"));
            }
            if let Some(error) = &entry.last_error {
                lines.push(format!("  Error: {error}"));
            }
            lines.push(format!("  Created: {}", entry.created_at));
            Ok(lines.join("\n"))
        })
        .collect::<Result<Vec<_>>>()
        .map(|entries| entries.join("\n\n"))
}

pub(crate) fn render_activity_history(
    activity: &[serde_json::Value],
    json: bool,
) -> Result<String> {
    if json {
        Ok(serde_json::to_string_pretty(activity)?)
    } else {
        activity_lines(activity)
    }
}

impl Runtime {
    pub fn wallet_activity(
        &self,
        name: &InstanceName,
        limit: Option<u32>,
        json: bool,
    ) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        self.wallet_activity_with(
            name,
            &dashboard,
            &client,
            std::io::stdout().lock(),
            limit,
            json,
        )
    }

    pub(crate) fn wallet_activity_with(
        &self,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        mut out: impl std::io::Write,
        limit: Option<u32>,
        json: bool,
    ) -> Result<()> {
        let activity = activity_history(dashboard, client, name, limit)?;
        writeln!(out, "{}", render_activity_history(&activity, json)?)?;
        Ok(())
    }

    pub fn wallet_balances(&self, name: &InstanceName, json: bool) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        let accounts = account_balances(&dashboard, &client, name)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&accounts)?);
        } else {
            println!("{}", balance_lines(&accounts));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Account {
    pub(crate) id: u8,
    pub(crate) transparent_address: String,
    pub(crate) unified_address: String,
    pub(crate) transparent_zatoshi: u64,
    pub(crate) ironwood_zatoshi: u64,
}

#[derive(Debug, Deserialize)]
struct ServerStatus {
    wallet_sync: WalletSyncStatus,
}

#[derive(Debug, Deserialize)]
struct WalletSyncStatus {
    state: String,
    error: Option<String>,
}

pub(crate) fn account_balances(
    dashboard: &str,
    client: &reqwest::blocking::Client,
    name: &InstanceName,
) -> Result<Vec<Account>> {
    let status: ServerStatus = client
        .get(format!("{dashboard}/api/v1/status"))
        .send()
        .map_err(anyhow::Error::from)
        .and_then(decode_response)
        .with_context(|| format!("checking wallet sync for environment {name}"))?;
    match status.wallet_sync.state.as_str() {
        "ready" => {}
        "syncing" => bail!("wallet for environment {name} is still syncing; try again later"),
        "error" => bail!(
            "wallet synchronization failed for environment {name}: {}",
            status
                .wallet_sync
                .error
                .as_deref()
                .unwrap_or("no error detail was provided")
        ),
        state => bail!("wallet for environment {name} has unknown sync state `{state}`"),
    }
    client
        .get(format!("{dashboard}/api/v1/accounts"))
        .send()
        .map_err(anyhow::Error::from)
        .and_then(decode_response)
        .with_context(|| format!("asking environment {name} for account balances"))
}

pub(crate) fn balance_lines(accounts: &[Account]) -> String {
    accounts
        .iter()
        .map(|account| {
            format!(
                "Account {}\n  Transparent  {} ZEC  {}\n  Ironwood     {} ZEC  {}",
                account.id,
                format_zec(account.transparent_zatoshi),
                account.transparent_address,
                format_zec(account.ironwood_zatoshi),
                account.unified_address
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RuntimeStatus {
    pub(crate) node_height: Option<u64>,
    pub(crate) lightwalletd_height: Option<u64>,
    pub(crate) wallet_scanned_height: Option<u64>,
    pub(crate) wallet_sync_state: String,
    pub(crate) settled: bool,
    pub(crate) settlement_blocker: Option<String>,
}
pub(crate) fn runtime_status(endpoints: &Endpoints, timeout: Duration) -> Result<RuntimeStatus> {
    let response = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()?
        .get(format!("{}/api/v1/status", endpoints.dashboard))
        .send()
        .context("reading runtime status")?;
    let status = response.status();
    if !status.is_success() {
        bail!("runtime status returned {status}");
    }
    response.json().context("decoding runtime status")
}
