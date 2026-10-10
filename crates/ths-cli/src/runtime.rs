use std::{
    collections::BTreeMap,
    fmt::{self, Display},
    fs::{self, File, OpenOptions, TryLockError},
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    str::FromStr,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const APP_IMAGE_REPOSITORY: &str = "ghcr.io/zcashlabs/thus-spoke-zakura-app";
const ZAKURA_IMAGE: &str = "zakuracore/zakura:1.6.0";
const LIGHTWALLETD_IMAGE_REPOSITORY: &str = "ghcr.io/zcashlabs/thus-spoke-zakura-lightwalletd";
const INSTANCE_LABEL: &str = "com.zakura.ths.instance";

fn app_image() -> String {
    format!("{APP_IMAGE_REPOSITORY}:{}", env!("CARGO_PKG_VERSION"))
}

fn lightwalletd_image() -> String {
    format!(
        "{LIGHTWALLETD_IMAGE_REPOSITORY}:{}",
        env!("CARGO_PKG_VERSION")
    )
}

#[derive(Clone, Debug)]
pub struct InstanceName(String);

impl Display for InstanceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for InstanceName {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let valid = !value.is_empty()
            && value.len() <= 40
            && value
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid || value.starts_with('-') || value.ends_with('-') {
            bail!("instance names use 1-40 lowercase letters, digits, or internal hyphens");
        }
        Ok(Self(value.to_owned()))
    }
}

fn default_regtest() -> String {
    "regtest".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoints {
    pub dashboard: String,
    pub rpc: String,
    pub lightwalletd: String,
    pub p2p: String,
    #[serde(default = "default_regtest")]
    pub network: String,
    #[serde(default)]
    pub tls: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Instance {
    name: String,
    version: u32,
    endpoints: Endpoints,
}

#[derive(Debug, Deserialize, Serialize)]
struct MineResult {
    blocks: usize,
    hashes: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FaucetResult {
    address: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    #[serde(default = "confirmed_status")]
    status: String,
}

fn confirmed_status() -> String {
    "confirmed".to_owned()
}

#[derive(Debug, Deserialize, Serialize)]
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
}

pub struct Runtime {
    root: PathBuf,
}

struct FaucetJournal {
    path: PathBuf,
    _lock: File,
}

impl FaucetJournal {
    fn open(instance_dir: &std::path::Path) -> Result<Self> {
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(instance_dir.join("faucet-intents.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                bail!(
                    "another faucet command is running for this environment; retry after it finishes"
                )
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).context("locking faucet intents");
            }
        }
        Ok(Self {
            path: instance_dir.join("faucet-intents.json"),
            _lock: lock,
        })
    }

    fn read(&self) -> Result<BTreeMap<String, String>> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("invalid saved faucet intents"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error).context("reading saved faucet intents"),
        }
    }

    fn write(&self, entries: &BTreeMap<String, String>) -> Result<()> {
        let temporary = self.path.with_extension("json.tmp");
        let file = File::create(&temporary)?;
        serde_json::to_writer(&file, entries)?;
        file.sync_all()?;
        fs::rename(&temporary, &self.path)?;
        File::open(
            self.path
                .parent()
                .context("faucet intent directory is missing")?,
        )?
        .sync_all()?;
        Ok(())
    }

    fn key_for(&self, intent: &str) -> Result<String> {
        let mut entries = self.read()?;
        if let Some(key) = entries.get(intent) {
            return Ok(key.clone());
        }
        let key = uuid::Uuid::new_v4().to_string();
        entries.insert(intent.to_owned(), key.clone());
        self.write(&entries)?;
        Ok(key)
    }

    fn wallet_batch(&self, pool: &str, amount: u64, accounts: &[u8]) -> Result<String> {
        let mut canonical = accounts.to_vec();
        canonical.sort_unstable();
        if canonical.is_empty() || canonical.windows(2).any(|pair| pair[0] == pair[1]) {
            bail!("accounts must contain unique account indices");
        }
        let account_list = canonical
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let prefix = format!("wallet:{pool}:{amount}:");
        for intent in self.read()?.keys() {
            if let Some((prior_list, _)) = intent
                .strip_prefix(&prefix)
                .and_then(|rest| rest.rsplit_once(':'))
                && prior_list != account_list
                && let Some(overlap) = account_list
                    .split(',')
                    .find(|id| prior_list.split(',').any(|prior| prior == *id))
            {
                bail!(
                    "account {overlap} belongs to an unfinished faucet batch; retry with --accounts {prior_list}"
                );
            }
        }
        Ok(format!("{prefix}{account_list}"))
    }

    fn clear(&self, intents: &[&str]) -> Result<()> {
        let mut entries = self.read()?;
        for intent in intents {
            entries.remove(*intent);
        }
        self.write(&entries)
    }
}

impl Runtime {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("com", "zakura", "thus-spoke-zakura")
            .ok_or_else(|| anyhow!("could not determine the platform configuration directory"))?;
        Ok(Self {
            root: dirs.config_dir().to_owned(),
        })
    }

    pub fn doctor(&self, json: bool) -> Result<()> {
        let docker = docker_output(["version", "--format", "{{.Server.Version}}"]);
        let result = serde_json::json!({
            "docker": docker.as_ref().ok(),
            "config_dir": self.root,
            "ok": docker.is_ok(),
        });
        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else if let Ok(version) = docker {
            println!("✓ Docker {version}\n✓ Config: {}", self.root.display());
        } else {
            bail!("Docker is not reachable; start Docker Desktop or the Docker daemon");
        }
        Ok(())
    }

    pub fn build(&self, dev: bool) -> Result<()> {
        self.doctor(false)?;
        build_project_images(dev)?;
        ensure_image(ZAKURA_IMAGE)?;
        println!("Runtime images are ready.");
        Ok(())
    }

    pub fn pull(&self) -> Result<()> {
        self.doctor(false)?;
        for image in [app_image(), lightwalletd_image(), ZAKURA_IMAGE.to_owned()] {
            println!("Pulling {image}…");
            docker(["pull", &image])?;
        }
        println!("Runtime images are ready.");
        Ok(())
    }

    pub fn start(
        &self,
        name: &InstanceName,
        no_open: bool,
        json: bool,
        port_offset: u16,
    ) -> Result<()> {
        host_ports(port_offset)?;
        self.doctor(false)?;
        for image in [app_image(), lightwalletd_image(), ZAKURA_IMAGE.to_owned()] {
            require_image(&image)?;
        }
        let shutdown = Shutdown::install()?;
        self.start_with(name, no_open, json, port_offset, &DockerHost, &shutdown)
    }

    pub fn status(&self, name: &InstanceName, json: bool) -> Result<()> {
        let endpoints = self
            .read_instance(name)
            .map(|i| i.endpoints)
            .or_else(|_| inspect_endpoints(&prefix(name)))?;
        let running = container_running(&format!("{}-app", prefix(name)))?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"name": name.to_string(), "running": running, "endpoints": endpoints})
                )?
            );
        } else {
            println!("{}", status_text(name, running, &endpoints));
        }
        Ok(())
    }

    pub fn endpoints(&self, name: &InstanceName, json: bool) -> Result<()> {
        let endpoints = self.read_instance(name)?.endpoints;
        if json {
            println!("{}", serde_json::to_string_pretty(&endpoints)?);
        } else {
            println!("{}", endpoint_lines(&endpoints));
        }
        Ok(())
    }

    pub fn open(&self, name: &InstanceName) -> Result<()> {
        open_url(&self.read_instance(name)?.endpoints.dashboard)
    }

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
        let result: MineResult = client
            .post(format!("{dashboard}/api/v1/mine"))
            .json(&serde_json::json!({"blocks": blocks}))
            .send()
            .map_err(anyhow::Error::from)
            .and_then(decode_response)
            .with_context(|| format!("asking environment {name} to mine {blocks} blocks"))?;
        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!("Mined {} blocks on {name}.", result.blocks);
            if let Some(tip) = result.hashes.last() {
                println!("New tip: {tip}");
            }
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
        let journal = FaucetJournal::open(&self.instance_dir(name))?;
        let intent = format!("address:{address}:{amount_zatoshi}");
        let idempotency_key = journal.key_for(&intent)?;
        let result: FaucetResult = client
            .post(format!("{dashboard}/api/v1/faucet/address"))
            .json(&serde_json::json!({
                "address": address,
                "amount_zatoshi": amount_zatoshi,
                "idempotency_key": idempotency_key,
            }))
            .send()
            .map_err(anyhow::Error::from)
            .and_then(decode_response)
            .with_context(|| format!("asking environment {name} to fund {address}"))?;
        if result.status == "confirmed" && result.block_hash.is_none() {
            bail!("faucet reported confirmation without a block hash; retry the same command");
        }
        if result.status == "confirmed" {
            journal.clear(&[&intent])?;
        }
        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!("Transaction: {}", result.txid);
            if let Some(block_hash) = &result.block_hash {
                println!(
                    "Sent {} ZEC to {} on {name}.",
                    format_zec(result.amount_zatoshi),
                    result.address
                );
                println!("Confirmed in: {block_hash}");
            } else {
                println!(
                    "Payment to {} is pending. Run the same command to check it again.",
                    result.address
                );
            }
        }
        if result.status == "confirmed" {
            Ok(())
        } else {
            bail!("faucet payment is pending; run the same command to check it again")
        }
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
            accounts,
            amount_zatoshi,
            pool,
            json,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn wallet_faucet_with(
        &self,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        accounts: &[u8],
        amount_zatoshi: u64,
        pool: &str,
        json: bool,
    ) -> Result<()> {
        let journal = FaucetJournal::open(&self.instance_dir(name))?;
        let batch = journal.wallet_batch(pool, amount_zatoshi, accounts)?;
        let intents: Vec<_> = accounts.iter().map(|id| format!("{batch}:{id}")).collect();
        let mut funded = Vec::new();
        let mut pending = Vec::new();
        let mut failures = Vec::new();
        for (&account_id, intent) in accounts.iter().zip(&intents) {
            let idempotency_key = journal.key_for(intent)?;
            let outcome = client
                .post(format!("{dashboard}/api/v1/faucet"))
                .json(&serde_json::json!({
                    "account_id": account_id,
                    "pool": pool,
                    "amount_zatoshi": amount_zatoshi,
                    "idempotency_key": idempotency_key,
                }))
                .send()
                .with_context(|| format!("asking environment {name} to fund account {account_id}"));
            match outcome.and_then(decode_response::<Activity>) {
                Ok(activity) if activity.status == "confirmed" => funded.push(activity),
                Ok(activity) => pending.push(activity),
                Err(error) => failures.push(format!("account {account_id}: {error:#}")),
            }
        }
        if failures.is_empty() && pending.is_empty() {
            journal.clear(&intents.iter().map(String::as_str).collect::<Vec<_>>())?;
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"funded": funded, "pending": pending, "failed": failures})
                )?
            );
        } else {
            for activity in &funded {
                println!(
                    "Funded account {} with {} ZEC ({} pool) on {name}.",
                    activity.to_account,
                    format_zec(activity.amount_zatoshi),
                    activity.destination_pool
                );
                println!("  Transaction: {}", activity.txid);
            }
            for activity in &pending {
                eprintln!(
                    "Payment to account {} is pending ({}). Run the same command to check it again.",
                    activity.to_account, activity.txid
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
        amount_zatoshi: u64,
        memo: Option<&str>,
        json: bool,
    ) -> Result<()> {
        let (dashboard, client) = self.instance_client(name)?;
        let idempotency_key = format!("ths-wallet-send-{}", uuid::Uuid::new_v4());
        let activity: Activity = client
            .post(format!("{dashboard}/api/v1/send"))
            .json(&serde_json::json!({
                "from_account": from,
                "to_account": to,
                "source_pool": source_pool,
                "destination_pool": destination_pool,
                "amount_zatoshi": amount_zatoshi,
                "idempotency_key": idempotency_key,
                "memo": memo,
            }))
            .send()
            .map_err(anyhow::Error::from)
            .and_then(decode_response)
            .with_context(|| {
                format!("asking environment {name} to send from account {from} to account {to}")
            })?;
        if json {
            println!("{}", serde_json::to_string_pretty(&activity)?);
        } else {
            println!(
                "Sent {} ZEC from account {} ({} pool) to account {} ({} pool) on {name}.",
                format_zec(activity.amount_zatoshi),
                from,
                activity.source_pool,
                activity.to_account,
                activity.destination_pool
            );
            if let Some(memo) = memo {
                println!("Memo: {memo}");
            }
            println!("Transaction: {}", activity.txid);
            if let Some(block_hash) = &activity.block_hash {
                println!("Confirmed in: {block_hash}");
            }
        }
        Ok(())
    }

    pub fn logs(
        &self,
        name: &InstanceName,
        service: Option<&str>,
        follow: bool,
        tail: Option<u64>,
        head: Option<u64>,
    ) -> Result<()> {
        let service = service.unwrap_or("app");
        let container = format!("{}-{service}", prefix(name));
        if let Some(lines) = head {
            return print_log_head(&container, lines);
        }
        let args = logs_args(&container, follow, tail);
        docker_inherit(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }

    pub fn stop(&self, name: &InstanceName) -> Result<()> {
        self.delete_instance_resources(name)?;
        println!("Stopped and deleted {name} and all of its development data.");
        Ok(())
    }

    pub fn reset(&self, name: &InstanceName, force: bool) -> Result<()> {
        if !force {
            bail!("reset deletes chain, wallet, and seed data; repeat with --force");
        }
        self.delete_instance_resources(name)?;
        println!("Deleted {name}; its Docker volumes cannot be recovered.");
        Ok(())
    }

    pub fn list(&self, json: bool) -> Result<()> {
        let mut instances = Vec::new();
        if self.root.exists() {
            for entry in fs::read_dir(&self.root)? {
                let path = entry?.path().join("instance.json");
                if path.exists() {
                    instances.push(serde_json::from_slice::<Instance>(&fs::read(path)?)?);
                }
            }
        }
        if json {
            println!("{}", serde_json::to_string_pretty(&instances)?);
        } else if instances.is_empty() {
            println!("No environments yet.");
        } else {
            for i in instances {
                println!("{:<20} {}", i.name, i.endpoints.dashboard);
            }
        }
        Ok(())
    }

    fn instance_dir(&self, name: &InstanceName) -> PathBuf {
        self.root.join(name.to_string())
    }
    fn write_instance(&self, name: &InstanceName, endpoints: &Endpoints) -> Result<()> {
        let instance = Instance {
            name: name.to_string(),
            version: 1,
            endpoints: endpoints.clone(),
        };
        fs::write(
            self.instance_dir(name).join("instance.json"),
            serde_json::to_vec_pretty(&instance)?,
        )?;
        Ok(())
    }
    fn read_instance(&self, name: &InstanceName) -> Result<Instance> {
        let path = self.instance_dir(name).join("instance.json");
        serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("instance {name} does not exist"))?,
        )
        .context("invalid instance metadata")
    }

    fn delete_instance_resources(&self, name: &InstanceName) -> Result<()> {
        self.delete_instance_resources_with(name, &DockerCli)
    }

    fn delete_partial_instance_resources(&self, name: &InstanceName) -> Result<()> {
        self.delete_instance_resources_with_mode(name, &DockerCli, true)
    }

    fn delete_instance_resources_with(
        &self,
        name: &InstanceName,
        docker: &impl DockerResourceCommands,
    ) -> Result<()> {
        self.delete_instance_resources_with_mode(name, docker, false)
    }

    fn delete_instance_resources_with_mode(
        &self,
        name: &InstanceName,
        docker: &impl DockerResourceCommands,
        partial: bool,
    ) -> Result<()> {
        let prefix = prefix(name);
        let mut failures = Vec::new();
        let mut inspect = |kind: &str, target: &str| {
            let result = owned_resource(docker, kind, target, name)
                .with_context(|| format!("{kind} {target}"));
            if partial {
                match result {
                    Ok(resource) => Ok(resource),
                    Err(error) => {
                        failures.push(format!("{error:#}"));
                        Ok(None)
                    }
                }
            } else {
                result
            }
        };
        let containers = ["app", "lightwalletd", "zakura", "init"]
            .into_iter()
            .map(|service| {
                let target = format!("{prefix}-{service}");
                inspect("container", &target)
            })
            .collect::<Result<Vec<_>>>()?;
        let volumes = ["chain", "wallet", "lightwalletd", "config"]
            .into_iter()
            .map(|suffix| {
                let volume = format!("{prefix}-{suffix}");
                inspect("volume", &volume)
            })
            .collect::<Result<Vec<_>>>()?;
        let network = inspect("network", &prefix)?;

        for id in containers.into_iter().flatten() {
            if let Err(error) = docker.run(&["rm", "-f", &id]) {
                failures.push(format!("container {id}: {error}"));
            }
        }
        for volume in volumes.into_iter().flatten() {
            if let Err(error) = docker.run(&["volume", "rm", &volume]) {
                failures.push(format!("volume {volume}: {error}"));
            }
        }
        if let Some(id) = network
            && let Err(error) = docker.run(&["network", "rm", &id])
        {
            failures.push(format!("network {prefix}: {error}"));
        }
        if failures.is_empty() {
            let dir = self.instance_dir(name);
            if dir.exists() {
                fs::remove_dir_all(&dir)
                    .with_context(|| format!("removing metadata {}", dir.display()))?;
            }
            Ok(())
        } else {
            bail!(
                "could not delete every instance resource: {}",
                failures.join("; ")
            )
        }
    }
}

fn decode_response<T: serde::de::DeserializeOwned>(
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

#[derive(Debug)]
struct HostPorts {
    dashboard: u16,
    rpc: u16,
    p2p: u16,
    lightwalletd: u16,
}

fn host_ports(offset: u16) -> Result<HostPorts> {
    if !offset.is_multiple_of(10) {
        bail!("--port-offset must be a multiple of 10 (got {offset})");
    }
    Ok(HostPorts {
        dashboard: 32805u16
            .checked_add(offset)
            .with_context(|| format!("--port-offset {offset} overflows the dashboard port"))?,
        rpc: 18232u16
            .checked_add(offset)
            .with_context(|| format!("--port-offset {offset} overflows the Zakura RPC port"))?,
        p2p: 18233u16
            .checked_add(offset)
            .with_context(|| format!("--port-offset {offset} overflows the P2P port"))?,
        lightwalletd: 9067u16
            .checked_add(offset)
            .with_context(|| format!("--port-offset {offset} overflows the lightwalletd port"))?,
    })
}

fn loopback_publish(host: u16, container: u16) -> String {
    format!("127.0.0.1:{host}:{container}")
}

fn require_free_loopback(port: u16) -> Result<()> {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(_) => bail!("port {port} is already in use on 127.0.0.1"),
    }
}

fn prefix(name: &InstanceName) -> String {
    format!("ths-{name}")
}
fn label(name: &InstanceName) -> String {
    format!("{INSTANCE_LABEL}={name}")
}

trait DockerResourceCommands {
    fn output(&self, args: &[&str]) -> Result<String>;
    fn run(&self, args: &[&str]) -> Result<()>;
}

struct DockerCli;

impl DockerResourceCommands for DockerCli {
    fn output(&self, args: &[&str]) -> Result<String> {
        docker_output_args(args)
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        docker_inherit(args)
    }
}

fn owned_resource(
    docker: &impl DockerResourceCommands,
    kind: &str,
    target: &str,
    name: &InstanceName,
) -> Result<Option<String>> {
    let names = match kind {
        "container" => docker.output(&["container", "ls", "-a", "--format", "{{.Names}}"])?,
        "volume" => docker.output(&["volume", "ls", "--format", "{{.Name}}"])?,
        "network" => docker.output(&["network", "ls", "--format", "{{.Name}}"])?,
        _ => unreachable!("resource kind is fixed by the caller"),
    };
    let matches = names
        .lines()
        .filter(|candidate| candidate.trim() == target)
        .count();
    if matches == 0 {
        return Ok(None);
    }
    if matches > 1 {
        bail!(
            "Docker has multiple {kind} resources named {target}; refusing to reuse or delete them"
        );
    }
    let details = docker.output(&[kind, "inspect", target])?;
    let resources: serde_json::Value = serde_json::from_str(&details)
        .with_context(|| format!("decoding Docker {kind} {target}"))?;
    let items = resources
        .as_array()
        .with_context(|| format!("Docker returned invalid {kind} {target}"))?;
    if items.len() != 1 {
        bail!(
            "Docker returned {} {kind} resources named {target}",
            items.len()
        );
    }
    let resource = &items[0];
    let labels = if kind == "container" {
        &resource["Config"]["Labels"]
    } else {
        &resource["Labels"]
    };
    if labels[INSTANCE_LABEL].as_str() != Some(name.0.as_str()) {
        bail!(
            "Docker {kind} {target} is not owned by ths instance {name}; refusing to reuse or delete it"
        );
    }
    // Containers and networks have stable IDs; Docker identifies volumes by name.
    let id_field = if kind == "volume" { "Name" } else { "Id" };
    let id = resource[id_field]
        .as_str()
        .filter(|id| !id.is_empty())
        .with_context(|| format!("Docker {kind} {target} has no {id_field}"))?;
    Ok(Some(id.to_owned()))
}

fn ensure_network(prefix: &str, name: &InstanceName) -> Result<()> {
    ensure_network_with(prefix, name, &DockerCli)
}
fn ensure_network_with(
    prefix: &str,
    name: &InstanceName,
    docker: &impl DockerResourceCommands,
) -> Result<()> {
    if owned_resource(docker, "network", prefix, name)?.is_none() {
        docker.run(&["network", "create", "--label", &label(name), prefix])?;
        owned_resource(docker, "network", prefix, name)?
            .with_context(|| format!("Docker did not create network {prefix}"))?;
    }
    Ok(())
}
fn ensure_volume(volume: &str, name: &InstanceName) -> Result<()> {
    ensure_volume_with(volume, name, &DockerCli)
}
fn ensure_volume_with(
    volume: &str,
    name: &InstanceName,
    docker: &impl DockerResourceCommands,
) -> Result<()> {
    if owned_resource(docker, "volume", volume, name)?.is_none() {
        docker.run(&["volume", "create", "--label", &label(name), volume])?;
        owned_resource(docker, "volume", volume, name)?
            .with_context(|| format!("Docker did not create volume {volume}"))?;
    }
    Ok(())
}
fn ensure_zakura(prefix: &str, name: &InstanceName, ports: &HostPorts) -> Result<()> {
    let target = format!("{prefix}-zakura");
    if owned_resource(&DockerCli, "container", &target, name)?.is_none() {
        let rpc_bind = loopback_publish(ports.rpc, 18232);
        let p2p_bind = loopback_publish(ports.p2p, 18233);
        docker([
            "create",
            "--name",
            &target,
            "--network",
            prefix,
            "--network-alias",
            "zakura",
            "--label",
            &label(name),
            "-p",
            &rpc_bind,
            "-p",
            &p2p_bind,
            "-v",
            &format!("{prefix}-chain:/data"),
            "-v",
            &format!("{prefix}-config:/config:ro"),
            "-e",
            "CONFIG_FILE_PATH=/config/zakurad.toml",
            ZAKURA_IMAGE,
            "zakurad",
            "start",
        ])?;
    }
    Ok(())
}
fn ensure_lightwalletd(prefix: &str, name: &InstanceName, ports: &HostPorts) -> Result<()> {
    let target = format!("{prefix}-lightwalletd");
    if owned_resource(&DockerCli, "container", &target, name)?.is_none() {
        let image = lightwalletd_image();
        let lightwalletd_bind = loopback_publish(ports.lightwalletd, 9067);
        docker([
            "create",
            "--name",
            &target,
            "--network",
            prefix,
            "--network-alias",
            "lightwalletd",
            "--label",
            &label(name),
            "--user",
            "0:0",
            "-p",
            &lightwalletd_bind,
            "-v",
            &format!("{prefix}-lightwalletd:/var/lib/lightwalletd"),
            &image,
            "--no-tls-very-insecure",
            "--grpc-bind-addr",
            "0.0.0.0:9067",
            "--rpchost",
            "zakura",
            "--rpcport",
            "18232",
            "--rpcuser",
            "unused",
            "--rpcpassword",
            "unused",
            "--data-dir",
            "/var/lib/lightwalletd",
            "--log-file",
            "/dev/stdout",
        ])?;
    }
    Ok(())
}
fn ensure_app(prefix: &str, name: &InstanceName, ports: &HostPorts) -> Result<()> {
    let target = format!("{prefix}-app");
    if owned_resource(&DockerCli, "container", &target, name)?.is_none() {
        let public_rpc = format!("http://127.0.0.1:{}", ports.rpc);
        let public_lightwalletd = format!("http://127.0.0.1:{}", ports.lightwalletd);
        let public_p2p = format!("127.0.0.1:{}", ports.p2p);
        let dashboard_bind = loopback_publish(ports.dashboard, 8080);
        let image = app_image();
        docker([
            "create",
            "--name",
            &target,
            "--network",
            prefix,
            "--label",
            &label(name),
            "-p",
            &dashboard_bind,
            "-e",
            "THS_LISTEN=0.0.0.0:8080",
            "-e",
            "THS_ZAKURA_RPC=http://zakura:18232",
            "-e",
            "THS_LIGHTWALLETD=http://lightwalletd:9067",
            "-e",
            &format!("THS_INSTANCE={name}"),
            "-e",
            &format!("THS_PUBLIC_ZAKURA_RPC={public_rpc}"),
            "-e",
            &format!("THS_PUBLIC_LIGHTWALLETD={public_lightwalletd}"),
            "-e",
            &format!("THS_PUBLIC_P2P={public_p2p}"),
            "-v",
            &format!("{prefix}-wallet:/data"),
            &image,
            "serve",
            "--data-dir",
            "/data",
        ])?;
    }
    Ok(())
}

fn endpoints_for(ports: &HostPorts) -> Endpoints {
    Endpoints {
        dashboard: format!("http://127.0.0.1:{}", ports.dashboard),
        rpc: format!("http://127.0.0.1:{}", ports.rpc),
        lightwalletd: format!("http://127.0.0.1:{}", ports.lightwalletd),
        p2p: format!("127.0.0.1:{}", ports.p2p),
        network: default_regtest(),
        tls: false,
    }
}

fn inspect_endpoints(prefix: &str) -> Result<Endpoints> {
    Ok(Endpoints {
        dashboard: format!(
            "http://127.0.0.1:{}",
            published_port(&format!("{prefix}-app"), "8080/tcp")?
        ),
        rpc: format!(
            "http://127.0.0.1:{}",
            published_port(&format!("{prefix}-zakura"), "18232/tcp")?
        ),
        lightwalletd: format!(
            "http://127.0.0.1:{}",
            published_port(&format!("{prefix}-lightwalletd"), "9067/tcp")?
        ),
        p2p: format!(
            "127.0.0.1:{}",
            published_port(&format!("{prefix}-zakura"), "18233/tcp")?
        ),
        network: default_regtest(),
        tls: false,
    })
}
fn published_port(container: &str, port: &str) -> Result<u16> {
    docker_output([
        "inspect",
        "--format",
        &format!("{{{{(index (index .NetworkSettings.Ports \"{port}\") 0).HostPort}}}}"),
        container,
    ])?
    .parse()
    .context("Docker returned an invalid published port")
}
fn container_exists(name: &str) -> Result<bool> {
    docker_listed([
        "container",
        "ls",
        "--all",
        "--quiet",
        "--filter",
        &format!("name=^{name}$"),
    ])
}
/// `docker inspect` fails the same way for a missing object and an unreachable daemon, so
/// existence comes from a filtered listing: empty means absent, failure stays an error.
fn docker_listed<const N: usize>(args: [&str; N]) -> Result<bool> {
    Ok(!docker_output(args)?.is_empty())
}
fn ensure_image(image: &str) -> Result<()> {
    if docker_output(["image", "inspect", image]).is_err() {
        println!("Pulling {image}…");
        docker(["pull", image])?;
    }
    Ok(())
}
fn require_image(image: &str) -> Result<()> {
    if docker_output(["image", "inspect", image]).is_err() {
        bail!(
            "required image {image} is unavailable; run `ths pull` (or `ths build` from a source checkout) first"
        );
    }
    Ok(())
}
fn build_project_images(dev: bool) -> Result<()> {
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !project_root.join("Dockerfile").is_file()
        || !project_root
            .join("docker/lightwalletd.Dockerfile")
            .is_file()
    {
        bail!(
            "cannot build images: project source is unavailable at {}",
            project_root.display()
        );
    }

    let app_image = app_image();
    let lightwalletd_image = lightwalletd_image();
    if dev {
        println!("Building {app_image} with the Rust development profile…");
        docker_inherit_in(
            &[
                "build",
                "--build-arg",
                "RUST_PROFILE=dev-runtime",
                "-t",
                &app_image,
                ".",
            ],
            &project_root,
        )?;
    } else {
        println!("Building {app_image}…");
        docker_inherit_in(&["build", "-t", &app_image, "."], &project_root)?;
    }
    println!("Building {lightwalletd_image}…");
    docker_inherit_in(
        &[
            "build",
            "-f",
            "docker/lightwalletd.Dockerfile",
            "-t",
            &lightwalletd_image,
            ".",
        ],
        &project_root,
    )
}
struct Shutdown {
    flag: Arc<AtomicBool>,
    /// Lets a `DeletionMonitor` end `wait` through the same channel as Ctrl+C.
    sender: mpsc::Sender<Ended>,
    receiver: mpsc::Receiver<Ended>,
}

impl Shutdown {
    #[cfg(test)]
    fn channel() -> (mpsc::Sender<Ended>, Self) {
        let (sender, receiver) = mpsc::channel();
        let shutdown = Self {
            flag: Arc::new(AtomicBool::new(false)),
            sender: sender.clone(),
            receiver,
        };
        (sender, shutdown)
    }

    fn install() -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let flag = Arc::new(AtomicBool::new(false));
        let handler_flag = flag.clone();
        let handler_sender = sender.clone();
        ctrlc::set_handler(move || {
            handler_flag.store(true, Ordering::SeqCst);
            let _ = handler_sender.send(Ended::Interrupted);
        })
        .context("installing the shutdown signal handler")?;
        Ok(Self {
            flag,
            sender,
            receiver,
        })
    }

    fn try_interrupted(&self) -> bool {
        // Deletion notices only arrive while `wait` runs, which receives them itself.
        if matches!(self.receiver.try_recv(), Ok(Ended::Interrupted)) {
            self.flag.store(true, Ordering::SeqCst);
        }
        self.flag.load(Ordering::SeqCst)
    }

    fn check(&self) -> Result<()> {
        if self.try_interrupted() {
            bail!("interrupted");
        }
        Ok(())
    }

    /// Waits for a shutdown signal, or for a `DeletionMonitor` to report that another command
    /// (`ths stop` or `ths reset` from another shell) deleted the environment.
    fn wait(&self) -> Result<Ended> {
        // Not `try_interrupted`: it would drop a deletion notice that is already queued.
        if self.flag.load(Ordering::SeqCst) {
            return Ok(Ended::Interrupted);
        }
        let ended = self
            .receiver
            .recv()
            .context("waiting for a shutdown signal")?;
        if ended == Ended::Interrupted {
            self.flag.store(true, Ordering::SeqCst);
        }
        Ok(ended)
    }

    fn wait_timeout(&self, timeout: Duration) -> Result<()> {
        if self.try_interrupted() {
            bail!("interrupted");
        }
        match self.receiver.recv_timeout(timeout) {
            Ok(_) => {
                self.flag.store(true, Ordering::SeqCst);
                bail!("interrupted");
            }
            Err(RecvTimeoutError::Timeout) => Ok(()),
            Err(RecvTimeoutError::Disconnected) => {
                if self.try_interrupted() {
                    bail!("interrupted");
                }
                Err(anyhow!("waiting for a shutdown signal"))
            }
        }
    }
}

/// How a running environment's foreground `start` ended.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    /// Ctrl+C or a termination signal: `start` deletes the environment itself.
    Interrupted,
    /// Another command already deleted it.
    DeletedElsewhere,
}

/// How long deletion monitoring waits before retrying after a Docker error or a lost event stream.
const DELETION_WATCH_RETRY: Duration = Duration::from_secs(1);

/// The container whose deletion a running `start` watches. Its ID keeps a same-name replacement
/// from hiding the deletion, and its creation time (Docker's clock) lets a subscription replay
/// a destroy event that happened before it connected.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WatchedContainer {
    id: String,
    created: String,
}

/// The Docker operations behind deletion monitoring, and its test seam.
trait ContainerEvents: Send + Sync {
    /// Identifies the container named `name`, or returns `None` once it no longer exists.
    fn identify(&self, name: &str) -> Result<Option<WatchedContainer>>;
    /// Whether the container with this ID still exists.
    fn exists(&self, id: &str) -> Result<bool>;
    /// Blocks until Docker reports that the container was destroyed (`Ok(true)`), or until the
    /// event stream ends without reporting it (`Ok(false)`).
    fn wait_destroyed(&self, container: &WatchedContainer) -> Result<bool>;
    /// Ends a blocked `wait_destroyed` and makes later ones return at once.
    fn cancel(&self);
}

/// Follows the app container's lifecycle through `docker events`.
#[derive(Default)]
struct DockerEvents {
    cancelled: AtomicBool,
    listener: Mutex<Option<Child>>,
}

impl DockerEvents {
    fn stop_listener(&self) {
        let listener = self
            .listener
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(mut child) = listener {
            // `kill` fails only when the stream already ended; `wait` reaps the process either way.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl ContainerEvents for DockerEvents {
    fn identify(&self, name: &str) -> Result<Option<WatchedContainer>> {
        if !container_exists(name)? {
            return Ok(None);
        }
        let details = docker_output([
            "container",
            "inspect",
            "--format",
            "{{.Id}} {{.Created}}",
            name,
        ])?;
        let (id, created) = details
            .split_once(' ')
            .ok_or_else(|| anyhow!("Docker returned invalid container details: {details}"))?;
        Ok(Some(WatchedContainer {
            id: id.to_owned(),
            created: created.to_owned(),
        }))
    }

    fn exists(&self, id: &str) -> Result<bool> {
        docker_listed([
            "container",
            "ls",
            "--all",
            "--quiet",
            "--filter",
            &format!("id={id}"),
        ])
    }

    fn wait_destroyed(&self, container: &WatchedContainer) -> Result<bool> {
        let stdout = {
            let mut listener = self.listener.lock().unwrap_or_else(PoisonError::into_inner);
            // Checked under the lock so `cancel` either sees this listener or stops it starting.
            if self.cancelled.load(Ordering::SeqCst) {
                return Ok(false);
            }
            let mut child = docker_cli()
                .args([
                    "events",
                    "--since",
                    &container.created,
                    "--filter",
                    &format!("container={}", container.id),
                    "--filter",
                    "event=destroy",
                    "--format",
                    "{{.Action}}",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("running Docker")?;
            let stdout = child
                .stdout
                .take()
                .context("reading Docker container events")?;
            *listener = Some(child);
            stdout
        };
        let destroyed = BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .any(|action| action.trim() == "destroy");
        self.stop_listener();
        Ok(destroyed)
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.stop_listener();
    }
}

/// Returns whether the container named `name` was deleted, or `false` once `stop` disconnects.
/// A Docker error never counts as deletion: the state is unknown, so it retries after `retry`.
fn watch_deletion(
    events: &dyn ContainerEvents,
    name: &str,
    retry: Duration,
    stop: &mpsc::Receiver<()>,
) -> bool {
    let stopped = || !matches!(stop.recv_timeout(retry), Err(RecvTimeoutError::Timeout));
    let container = loop {
        match events.identify(name) {
            Ok(Some(container)) => break container,
            Ok(None) => return true,
            Err(_) if stopped() => return false,
            Err(_) => {}
        }
    };
    loop {
        if matches!(events.wait_destroyed(&container), Ok(true)) {
            return true;
        }
        if !matches!(stop.try_recv(), Err(TryRecvError::Empty)) {
            return false;
        }
        // The stream ended or failed, for example when the daemon restarted. Its buffered
        // events may be gone, so check the container itself before subscribing again.
        match events.exists(&container.id) {
            Ok(false) => return true,
            Ok(true) | Err(_) if stopped() => return false,
            Ok(true) | Err(_) => {}
        }
    }
}

/// Watches the app container on a background thread and reports its deletion to `Shutdown`.
/// Dropping it stops the watch and reaps its `docker events` process.
struct DeletionMonitor {
    events: Arc<dyn ContainerEvents>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl DeletionMonitor {
    fn spawn(
        events: Arc<dyn ContainerEvents>,
        app_container: &str,
        retry: Duration,
        shutdown: &Shutdown,
    ) -> Self {
        let (stop, stopped) = mpsc::channel();
        let watcher = events.clone();
        let name = app_container.to_owned();
        let notify = shutdown.sender.clone();
        let thread = thread::spawn(move || {
            if watch_deletion(watcher.as_ref(), &name, retry, &stopped) {
                // Fails only once `start` has stopped listening, when the notice no longer matters.
                let _ = notify.send(Ended::DeletedElsewhere);
            }
        });
        Self {
            events,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for DeletionMonitor {
    fn drop(&mut self) {
        drop(self.stop.take());
        self.events.cancel();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            eprintln!("deletion monitoring stopped unexpectedly");
        }
    }
}

trait StartHost {
    fn delete(&self, runtime: &Runtime, name: &InstanceName) -> Result<()>;
    fn delete_partial(&self, runtime: &Runtime, name: &InstanceName) -> Result<()> {
        self.delete(runtime, name)
    }
    fn allocate(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
        port_offset: u16,
    ) -> Result<Endpoints>;
    fn wait_ready(
        &self,
        endpoints: &Endpoints,
        app_container: &str,
        timeout: Duration,
        shutdown: &Shutdown,
    ) -> Result<()>;
    fn open_url(&self, url: &str) -> Result<()>;
    fn wait_for_shutdown(&self, app_container: &str, shutdown: &Shutdown) -> Result<Ended>;
}

struct DockerHost;

impl StartHost for DockerHost {
    fn delete(&self, runtime: &Runtime, name: &InstanceName) -> Result<()> {
        runtime.delete_instance_resources(name)
    }

    fn delete_partial(&self, runtime: &Runtime, name: &InstanceName) -> Result<()> {
        runtime.delete_partial_instance_resources(name)
    }

    fn allocate(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
        port_offset: u16,
    ) -> Result<Endpoints> {
        fs::create_dir_all(runtime.instance_dir(name))?;
        let prefix = prefix(name);
        let ports = host_ports(port_offset)?;
        require_free_loopback(ports.dashboard)?;
        require_free_loopback(ports.rpc)?;
        require_free_loopback(ports.p2p)?;
        require_free_loopback(ports.lightwalletd)?;
        ensure_network(&prefix, name)?;
        shutdown.check()?;
        for suffix in ["chain", "wallet", "lightwalletd", "config"] {
            ensure_volume(&format!("{prefix}-{suffix}"), name)?;
        }
        shutdown.check()?;

        if owned_resource(&DockerCli, "container", &format!("{prefix}-init"), name)?.is_none() {
            docker([
                "create",
                "--name",
                &format!("{prefix}-init"),
                "--label",
                &label(name),
                "-v",
                &format!("{prefix}-wallet:/data"),
                "-v",
                &format!("{prefix}-config:/config"),
                &app_image(),
                "init",
                "--data-dir",
                "/data",
                "--config-dir",
                "/config",
            ])?;
            shutdown.check()?;
            docker(["start", "-a", &format!("{prefix}-init")])?;
            shutdown.check()?;
        }

        ensure_zakura(&prefix, name, &ports)?;
        shutdown.check()?;
        ensure_lightwalletd(&prefix, name, &ports)?;
        shutdown.check()?;
        let zakura_container = format!("{prefix}-zakura");
        docker(["start", &zakura_container])?;
        shutdown.check()?;
        let zakura_rpc = format!(
            "http://127.0.0.1:{}",
            published_port(&zakura_container, "18232/tcp")?
        );
        wait_for_zakura_tip(
            &zakura_rpc,
            &zakura_container,
            Duration::from_secs(120),
            shutdown,
        )?;
        docker(["start", &format!("{prefix}-lightwalletd")])?;
        shutdown.check()?;
        ensure_app(&prefix, name, &ports)?;
        shutdown.check()?;
        docker(["start", &format!("{prefix}-app")])?;
        shutdown.check()?;
        let endpoints = endpoints_for(&ports);
        runtime.write_instance(name, &endpoints)?;
        Ok(endpoints)
    }

    fn wait_ready(
        &self,
        endpoints: &Endpoints,
        app_container: &str,
        timeout: Duration,
        shutdown: &Shutdown,
    ) -> Result<()> {
        wait_ready(&endpoints.dashboard, app_container, timeout, shutdown)
    }

    fn open_url(&self, url: &str) -> Result<()> {
        open_url(url)
    }

    fn wait_for_shutdown(&self, app_container: &str, shutdown: &Shutdown) -> Result<Ended> {
        let _monitor = DeletionMonitor::spawn(
            Arc::new(DockerEvents::default()),
            app_container,
            DELETION_WATCH_RETRY,
            shutdown,
        );
        shutdown.wait()
    }
}

struct CleanupOnDrop<'a> {
    runtime: &'a Runtime,
    name: &'a InstanceName,
    host: &'a dyn StartHost,
    active: bool,
}

impl Drop for CleanupOnDrop<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Err(error) = self.host.delete_partial(self.runtime, self.name) {
            eprintln!("could not delete {}: {error:#}", self.name);
        }
    }
}

impl Runtime {
    fn start_with(
        &self,
        name: &InstanceName,
        no_open: bool,
        json: bool,
        port_offset: u16,
        host: &dyn StartHost,
        shutdown: &Shutdown,
    ) -> Result<()> {
        host_ports(port_offset)?;
        let mut cleanup = CleanupOnDrop {
            runtime: self,
            name,
            host,
            active: false,
        };
        println!("Preparing a fresh {name} environment…");
        host.delete(self, name)?;
        cleanup.active = true;
        println!("Starting {name}…");
        let endpoints = host.allocate(self, name, shutdown, port_offset)?;
        shutdown.check()?;
        let app_container = format!("{}-app", prefix(name));
        host.wait_ready(
            &endpoints,
            &app_container,
            Duration::from_secs(120),
            shutdown,
        )?;
        if json {
            println!("{}", serde_json::to_string_pretty(&endpoints)?);
        } else {
            println!("\n{name} is ready 🌸\n{}", endpoint_lines(&endpoints));
        }
        if !no_open {
            host.open_url(&endpoints.dashboard)?;
        }
        if !json {
            println!("\nPress Ctrl+C to stop and delete this development environment.");
        }
        match host.wait_for_shutdown(&app_container, shutdown)? {
            Ended::Interrupted => {
                println!("\nStopping and deleting {name}…");
                host.delete(self, name)?;
                cleanup.active = false;
                println!("Deleted {name} and all of its development data.");
            }
            Ended::DeletedElsewhere => {
                cleanup.active = false;
                println!("\n{name} was stopped and deleted by another command.");
            }
        }
        Ok(())
    }
}

/// Lists only running containers, so a stopped or missing one is `false` rather than an error.
fn container_running(name: &str) -> Result<bool> {
    docker_listed([
        "container",
        "ls",
        "--quiet",
        "--filter",
        &format!("name=^{name}$"),
    ])
}
fn wait_ready(
    base: &str,
    app_container: &str,
    timeout: Duration,
    shutdown: &Shutdown,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        shutdown.check()?;
        if Command::new("curl")
            .args(["-fsS", &format!("{base}/api/v1/health")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return Ok(());
        }
        if !container_running(app_container)? {
            let logs = docker_logs(app_container)
                .unwrap_or_else(|error| format!("could not read app logs: {error}"));
            bail!("app exited before becoming healthy:\n{logs}");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        shutdown.wait_timeout(remaining.min(Duration::from_millis(750)))?;
    }
    bail!(
        "dashboard did not become healthy within {} seconds",
        timeout.as_secs()
    )
}
fn wait_for_zakura_tip(
    base: &str,
    container: &str,
    timeout: Duration,
    shutdown: &Shutdown,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        shutdown.check()?;
        let tip_available = Command::new("curl")
            .args([
                "-sS",
                "-H",
                "content-type: application/json",
                "--data",
                r#"{"jsonrpc":"2.0","id":1,"method":"getbestblockhash","params":[]}"#,
                base,
            ])
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| serde_json::from_slice::<serde_json::Value>(&output.stdout).ok())
            .and_then(|response| {
                response
                    .get("result")
                    .and_then(|result| result.as_str())
                    .map(str::to_owned)
            })
            .is_some();
        if tip_available {
            return Ok(());
        }
        if !container_running(container)? {
            let logs = docker_logs(container)
                .unwrap_or_else(|error| format!("could not read Zakura logs: {error}"));
            bail!("Zakura exited before its RPC tip became available:\n{logs}");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        shutdown.wait_timeout(remaining.min(Duration::from_millis(250)))?;
    }
    bail!(
        "Zakura RPC tip did not become available within {} seconds",
        timeout.as_secs()
    )
}
fn status_text(name: &InstanceName, running: bool, e: &Endpoints) -> String {
    let state = if running { "running" } else { "stopped" };
    format!("{name}: {state}\n{}", endpoint_lines(e))
}
fn endpoint_lines(e: &Endpoints) -> String {
    format!(
        "  Dashboard    {}\n  Zakura RPC   {}\n  lightwalletd {}  (network={}, tls={})\n  P2P          {}",
        e.dashboard, e.rpc, e.lightwalletd, e.network, e.tls, e.p2p
    )
}
fn open_url(url: &str) -> Result<()> {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else {
        ("xdg-open", vec![url])
    };
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("opening {url}"))?;
    Ok(())
}
fn docker_cli() -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new("docker");
    #[cfg(test)]
    if tests::DOCKER_UNREACHABLE.get() {
        command.env("DOCKER_HOST", "unix:///nonexistent/ths-tests/docker.sock");
    }
    command
}
fn docker<const N: usize>(args: [&str; N]) -> Result<()> {
    docker_inherit(&args)
}
fn logs_args(container: &str, follow: bool, tail: Option<u64>) -> Vec<String> {
    let mut args = vec!["logs".to_owned()];
    if follow {
        args.push("--follow".to_owned());
    }
    if let Some(lines) = tail {
        args.extend(["--tail".to_owned(), lines.to_string()]);
    }
    args.push(container.to_owned());
    args
}

/// docker has no head option, so stream both of its output streams through
/// one pipe and stop the `docker logs` process once enough lines have been printed.
fn print_log_head(container: &str, lines: u64) -> Result<()> {
    let (reader, writer) = std::io::pipe().context("creating log pipe")?;
    let mut child = Command::new("docker")
        .args(["logs", container])
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .spawn()
        .context("running Docker")?;
    let printed = copy_lines(
        std::io::BufReader::new(reader),
        std::io::stdout().lock(),
        lines,
    )?;
    if printed == lines {
        child.kill().context("stopping docker logs")?;
    }
    // a killed `docker logs` leaves no exit code, so only its own failures count
    if child.wait()?.code().is_some_and(|code| code != 0) {
        bail!("docker logs {container} failed");
    }
    Ok(())
}

fn copy_lines(
    mut reader: impl std::io::BufRead,
    mut out: impl std::io::Write,
    limit: u64,
) -> std::io::Result<u64> {
    let mut line = Vec::new();
    let mut copied = 0;
    while copied < limit {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        out.write_all(&line)?;
        copied += 1;
    }
    out.flush()?;
    Ok(copied)
}

fn docker_inherit(args: &[&str]) -> Result<()> {
    docker_command(args, None)
}
fn docker_inherit_in(args: &[&str], current_dir: &std::path::Path) -> Result<()> {
    docker_command(args, Some(current_dir))
}
fn docker_command(args: &[&str], current_dir: Option<&std::path::Path>) -> Result<()> {
    let mut command = docker_cli();
    command.args(args);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    let status = command.status().context("running Docker")?;
    if !status.success() {
        bail!("docker {} failed", args.join(" "));
    }
    Ok(())
}
fn docker_output<const N: usize>(args: [&str; N]) -> Result<String> {
    docker_output_args(&args)
}
fn docker_output_args(args: &[&str]) -> Result<String> {
    let output = docker_cli().args(args).output().context("running Docker")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
fn docker_logs(container: &str) -> Result<String> {
    let output = docker_cli()
        .args(["logs", "--tail", "50", container])
        .output()
        .context("running Docker")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let mut logs = output.stdout;
    logs.extend_from_slice(&output.stderr);
    Ok(String::from_utf8_lossy(&logs).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    thread_local! {
        /// Points this test thread's Docker commands at a daemon that does not exist.
        pub(super) static DOCKER_UNREACHABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn with_unreachable_docker<T>(test: impl FnOnce() -> T) -> T {
        DOCKER_UNREACHABLE.set(true);
        let result = test();
        DOCKER_UNREACHABLE.set(false);
        result
    }

    #[test]
    fn docker_errors_are_not_reported_as_missing_resources() {
        with_unreachable_docker(|| {
            assert!(container_exists("ths-alpha-app").is_err());
            assert!(container_running("ths-alpha-app").is_err());
            assert!(
                owned_resource(&DockerCli, "volume", "ths-alpha-chain", &name("alpha")).is_err()
            );
            assert!(owned_resource(&DockerCli, "network", "ths-alpha", &name("alpha")).is_err());
            assert!(DockerEvents::default().identify("ths-alpha-app").is_err());
            assert!(DockerEvents::default().exists("0123abcd").is_err());
        });
    }

    #[test]
    fn deleting_keeps_metadata_while_docker_is_unreachable() {
        let runtime = Runtime {
            root: std::env::temp_dir().join(format!("ths-unreachable-{}", std::process::id())),
        };
        let name = name("alpha");
        fs::create_dir_all(runtime.instance_dir(&name)).unwrap();
        runtime
            .write_instance(&name, &endpoints_for(&host_ports(0).unwrap()))
            .unwrap();
        let err = with_unreachable_docker(|| runtime.stop(&name)).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("container ths-alpha-app"),
            "container missing from {message}"
        );
        assert!(runtime.read_instance(&name).is_ok(), "metadata was deleted");
        fs::remove_dir_all(&runtime.root).unwrap();
    }

    #[test]
    fn commands_report_docker_errors_instead_of_a_stopped_environment() {
        let runtime = runtime_for_tests();
        let name = name("alpha");
        let err = with_unreachable_docker(|| runtime.mine(&name, 1, false)).unwrap_err();
        assert!(
            !err.to_string().contains("is not running"),
            "reported a Docker error as a stopped environment: {err}"
        );
    }

    use std::{collections::HashMap, sync::Mutex};

    #[test]
    fn builds_docker_logs_arguments() {
        assert_eq!(
            logs_args("ths-default-app", false, None),
            ["logs", "ths-default-app"]
        );
        assert_eq!(
            logs_args("ths-default-zakura", true, None),
            ["logs", "--follow", "ths-default-zakura"]
        );
        assert_eq!(
            logs_args("ths-default-app", false, Some(200)),
            ["logs", "--tail", "200", "ths-default-app"]
        );
        assert_eq!(
            logs_args("ths-default-lightwalletd", true, Some(200)),
            [
                "logs",
                "--follow",
                "--tail",
                "200",
                "ths-default-lightwalletd"
            ]
        );
    }

    #[test]
    fn copies_only_the_requested_leading_lines() {
        let mut out = Vec::new();
        let mut input = std::io::Cursor::new("one\ntwo\nthree\n");
        assert_eq!(copy_lines(&mut input, &mut out, 2).unwrap(), 2);
        assert_eq!(out, b"one\ntwo\n");
        assert_eq!(input.position(), 8, "read past the requested lines");

        let mut out = Vec::new();
        let input = std::io::Cursor::new("one\ntwo");
        assert_eq!(copy_lines(input, &mut out, 5).unwrap(), 2);
        assert_eq!(out, b"one\ntwo");
    }

    struct RecordingDocker {
        output: HashMap<String, String>,
        runs: Mutex<Vec<String>>,
    }

    impl RecordingDocker {
        fn new(containers: &str, volumes: &str, networks: &str) -> Self {
            Self {
                output: HashMap::from([
                    (
                        "container ls -a --format {{.Names}}".into(),
                        containers.into(),
                    ),
                    ("volume ls --format {{.Name}}".into(), volumes.into()),
                    ("network ls --format {{.Name}}".into(), networks.into()),
                ]),
                runs: Mutex::new(Vec::new()),
            }
        }

        fn inspect(&mut self, kind: &str, target: &str, body: &str) {
            self.output
                .insert(format!("{kind} inspect {target}"), body.into());
        }
    }

    impl DockerResourceCommands for RecordingDocker {
        fn output(&self, args: &[&str]) -> Result<String> {
            let command = args.join(" ");
            self.output
                .get(&command)
                .cloned()
                .ok_or_else(|| anyhow!("unexpected Docker read: {command}"))
        }

        fn run(&self, args: &[&str]) -> Result<()> {
            self.runs.lock().unwrap().push(args.join(" "));
            Ok(())
        }
    }

    #[test]
    fn foreign_volume_collision_preserves_all_resources() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        let mut docker = RecordingDocker::new("ths-alpha-app", "ths-alpha-wallet", "");
        docker.inspect(
            "container",
            "ths-alpha-app",
            r#"[{"Id":"owned-container","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
        );
        docker.inspect(
            "volume",
            "ths-alpha-wallet",
            r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"other"}}]"#,
        );

        let error = runtime
            .delete_instance_resources_with(&name("alpha"), &docker)
            .unwrap_err();

        assert!(format!("{error:#}").contains("not owned"), "{error:#}");
        assert!(docker.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn partial_startup_cleanup_removes_only_owned_resources() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        let metadata = runtime.instance_dir(&name("alpha"));
        fs::create_dir_all(&metadata).unwrap();
        fs::write(metadata.join("instance.json"), "partial startup").unwrap();
        let mut docker = RecordingDocker::new("", "ths-alpha-chain\nths-alpha-wallet", "ths-alpha");
        docker.inspect(
            "volume",
            "ths-alpha-chain",
            r#"[{"Name":"ths-alpha-chain","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
        );
        docker.inspect(
            "volume",
            "ths-alpha-wallet",
            r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"other"}}]"#,
        );
        docker.inspect(
            "network",
            "ths-alpha",
            r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
        );

        let error = runtime
            .delete_instance_resources_with_mode(&name("alpha"), &docker, true)
            .unwrap_err();

        assert!(format!("{error:#}").contains("not owned"), "{error:#}");
        assert_eq!(
            *docker.runs.lock().unwrap(),
            ["volume rm ths-alpha-chain", "network rm owned-network"]
        );
        assert_eq!(
            fs::read_to_string(metadata.join("instance.json")).unwrap(),
            "partial startup"
        );
    }

    #[test]
    fn foreign_container_collision_is_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        let mut docker = RecordingDocker::new("ths-alpha-app", "", "");
        docker.inspect(
            "container",
            "ths-alpha-app",
            r#"[{"Id":"foreign-container","Config":{"Labels":{"com.zakura.ths.instance":"other"}}}]"#,
        );

        let error = runtime
            .delete_instance_resources_with(&name("alpha"), &docker)
            .unwrap_err();

        assert!(format!("{error:#}").contains("not owned"), "{error:#}");
        assert!(docker.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn owned_resources_still_clean_up() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        let mut docker = RecordingDocker::new("ths-alpha-app", "ths-alpha-wallet", "ths-alpha");
        docker.inspect(
            "container",
            "ths-alpha-app",
            r#"[{"Id":"owned-container","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
        );
        docker.inspect(
            "volume",
            "ths-alpha-wallet",
            r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
        );
        docker.inspect(
            "network",
            "ths-alpha",
            r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
        );

        runtime
            .delete_instance_resources_with(&name("alpha"), &docker)
            .unwrap();

        assert_eq!(
            *docker.runs.lock().unwrap(),
            [
                "rm -f owned-container",
                "volume rm ths-alpha-wallet",
                "network rm owned-network",
            ]
        );
    }

    #[test]
    fn unlabeled_network_collision_preserves_data_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        let metadata = runtime.instance_dir(&name("alpha"));
        fs::create_dir_all(&metadata).unwrap();
        fs::write(metadata.join("instance.json"), "important data").unwrap();
        let mut docker = RecordingDocker::new("ths-alpha-app", "ths-alpha-wallet", "ths-alpha");
        docker.inspect(
            "container",
            "ths-alpha-app",
            r#"[{"Id":"owned-container","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
        );
        docker.inspect(
            "volume",
            "ths-alpha-wallet",
            r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
        );
        docker.inspect(
            "network",
            "ths-alpha",
            r#"[{"Id":"old-network","Labels":{}}]"#,
        );

        let error = runtime
            .delete_instance_resources_with(&name("alpha"), &docker)
            .unwrap_err();

        assert!(format!("{error:#}").contains("not owned"), "{error:#}");
        assert!(docker.runs.lock().unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(metadata.join("instance.json")).unwrap(),
            "important data"
        );
    }

    #[test]
    fn new_networks_are_labeled_for_owned_cleanup() {
        struct NewNetwork(AtomicBool);

        impl DockerResourceCommands for NewNetwork {
            fn output(&self, args: &[&str]) -> Result<String> {
                match args {
                    ["network", "ls", "--format", "{{.Name}}"] => {
                        Ok(if self.0.load(Ordering::SeqCst) {
                            "ths-alpha".into()
                        } else {
                            String::new()
                        })
                    }
                    ["network", "inspect", "ths-alpha"] => Ok(
                        r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#
                            .into(),
                    ),
                    _ => bail!("unexpected Docker read: {args:?}"),
                }
            }

            fn run(&self, args: &[&str]) -> Result<()> {
                assert_eq!(
                    args,
                    [
                        "network",
                        "create",
                        "--label",
                        "com.zakura.ths.instance=alpha",
                        "ths-alpha"
                    ]
                );
                self.0.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        let docker = NewNetwork(AtomicBool::new(false));

        ensure_network_with("ths-alpha", &name("alpha"), &docker).unwrap();
        assert!(docker.0.load(Ordering::SeqCst));
    }

    #[test]
    fn duplicate_network_names_are_rejected() {
        let docker = RecordingDocker::new("", "", "ths-alpha\nths-alpha");

        let error = ensure_network_with("ths-alpha", &name("alpha"), &docker).unwrap_err();

        assert!(
            error.to_string().contains("multiple network resources"),
            "{error:#}"
        );
        assert!(docker.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn volume_create_does_not_accept_a_racing_foreign_volume() {
        struct RacingVolume(AtomicBool);

        impl DockerResourceCommands for RacingVolume {
            fn output(&self, args: &[&str]) -> Result<String> {
                match args {
                    ["volume", "ls", "--format", "{{.Name}}"] => Ok(if self.0.load(Ordering::SeqCst) {
                        "ths-alpha-wallet".into()
                    } else {
                        String::new()
                    }),
                    ["volume", "inspect", "ths-alpha-wallet"] => Ok(
                        r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"other"}}]"#.into(),
                    ),
                    _ => bail!("unexpected Docker read: {args:?}"),
                }
            }

            fn run(&self, args: &[&str]) -> Result<()> {
                assert_eq!(
                    args,
                    [
                        "volume",
                        "create",
                        "--label",
                        "com.zakura.ths.instance=alpha",
                        "ths-alpha-wallet"
                    ]
                );
                self.0.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        let error = ensure_volume_with(
            "ths-alpha-wallet",
            &name("alpha"),
            &RacingVolume(AtomicBool::new(false)),
        )
        .unwrap_err();

        assert!(error.to_string().contains("not owned"), "{error:#}");
    }

    struct RecordingHost {
        events: Arc<Mutex<Vec<String>>>,
        initial_delete_error: bool,
        shutdown_collision: Option<RecordingDocker>,
        wait_ready_result: Result<(), String>,
        open_url_result: Result<(), String>,
        interrupt_before_ready: bool,
        deleted_elsewhere: bool,
    }

    impl RecordingHost {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    events: events.clone(),
                    initial_delete_error: false,
                    shutdown_collision: None,
                    wait_ready_result: Ok(()),
                    open_url_result: Ok(()),
                    interrupt_before_ready: false,
                    deleted_elsewhere: false,
                },
                events,
            )
        }

        fn push(&self, event: &str) {
            self.events.lock().unwrap().push(event.to_owned());
        }
    }

    impl StartHost for RecordingHost {
        fn delete(&self, runtime: &Runtime, name: &InstanceName) -> Result<()> {
            self.push(&format!("delete:{name}"));
            if self.initial_delete_error {
                bail!("resource collision");
            }
            if self
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.starts_with("wait_for_shutdown"))
                && let Some(docker) = &self.shutdown_collision
            {
                return runtime.delete_instance_resources_with(name, docker);
            }
            Ok(())
        }

        fn delete_partial(&self, runtime: &Runtime, name: &InstanceName) -> Result<()> {
            if let Some(docker) = &self.shutdown_collision {
                self.push(&format!("delete_partial:{name}"));
                runtime.delete_instance_resources_with_mode(name, docker, true)
            } else {
                self.delete(runtime, name)
            }
        }

        fn allocate(
            &self,
            _runtime: &Runtime,
            name: &InstanceName,
            shutdown: &Shutdown,
            _port_offset: u16,
        ) -> Result<Endpoints> {
            self.push(&format!("allocate:{name}"));
            shutdown.check()?;
            Ok(Endpoints {
                dashboard: "http://127.0.0.1:1".into(),
                rpc: "http://127.0.0.1:2".into(),
                lightwalletd: "http://127.0.0.1:3".into(),
                p2p: "127.0.0.1:4".into(),
                network: default_regtest(),
                tls: false,
            })
        }

        fn wait_ready(
            &self,
            _endpoints: &Endpoints,
            _app_container: &str,
            _timeout: Duration,
            shutdown: &Shutdown,
        ) -> Result<()> {
            self.push("wait_ready");
            if self.interrupt_before_ready {
                bail!("interrupted");
            }
            shutdown.check()?;
            self.wait_ready_result
                .as_ref()
                .map(|_| ())
                .map_err(|e| anyhow!("{e}"))
        }

        fn open_url(&self, url: &str) -> Result<()> {
            self.push(&format!("open_url:{url}"));
            self.open_url_result
                .as_ref()
                .map(|_| ())
                .map_err(|e| anyhow!("{e}"))
        }

        fn wait_for_shutdown(&self, app_container: &str, shutdown: &Shutdown) -> Result<Ended> {
            self.push(&format!("wait_for_shutdown:{app_container}"));
            if self.shutdown_collision.is_some() {
                return Ok(Ended::Interrupted);
            }
            let events = ScriptedEvents::default();
            let app = (!self.deleted_elsewhere).then(|| watched("app-1"));
            events.identify.lock().unwrap().push_back(Ok(app));
            let _monitor = DeletionMonitor::spawn(
                Arc::new(events),
                app_container,
                Duration::from_millis(1),
                shutdown,
            );
            shutdown.wait()
        }
    }

    fn runtime_for_tests() -> Runtime {
        Runtime {
            root: std::env::temp_dir().join("ths-start-cleanup-tests"),
        }
    }

    fn name(value: &str) -> InstanceName {
        value.parse().unwrap()
    }

    #[test]
    fn initial_cleanup_failure_does_not_retry_on_drop() {
        let (mut host, events) = RecordingHost::new();
        host.initial_delete_error = true;
        let (_sender, shutdown) = Shutdown::channel();

        let error = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
            .unwrap_err();

        assert!(error.to_string().contains("resource collision"));
        assert_eq!(*events.lock().unwrap(), ["delete:alpha"]);
    }

    #[test]
    fn shutdown_collision_removes_owned_resources_and_preserves_foreign_container() {
        let mut docker = RecordingDocker::new(
            "ths-alpha-app\nths-alpha-init",
            "ths-alpha-wallet",
            "ths-alpha",
        );
        docker.inspect(
            "container",
            "ths-alpha-app",
            r#"[{"Id":"owned-app","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
        );
        docker.inspect(
            "container",
            "ths-alpha-init",
            r#"[{"Id":"foreign-init","Config":{"Labels":{}}}]"#,
        );
        docker.inspect(
            "volume",
            "ths-alpha-wallet",
            r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
        );
        docker.inspect(
            "network",
            "ths-alpha",
            r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
        );
        let (mut host, events) = RecordingHost::new();
        host.shutdown_collision = Some(docker);
        let (_sender, shutdown) = Shutdown::channel();

        let error = runtime_for_tests()
            .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
            .unwrap_err();

        assert!(format!("{error:#}").contains("ths-alpha-init is not owned"));
        assert!(
            events
                .lock()
                .unwrap()
                .contains(&"delete_partial:alpha".into())
        );
        assert_eq!(
            *host
                .shutdown_collision
                .as_ref()
                .unwrap()
                .runs
                .lock()
                .unwrap(),
            [
                "rm -f owned-app",
                "volume rm ths-alpha-wallet",
                "network rm owned-network",
            ]
        );
    }

    #[test]
    fn faucet_keys_survive_cli_restart_until_the_whole_intent_is_confirmed() {
        let dir = std::env::temp_dir().join(format!("ths-faucet-journal-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let first = FaucetJournal::open(&dir).unwrap();
        let account_one = first.key_for("wallet:ironwood:100:1,2:1").unwrap();
        let account_two = first.key_for("wallet:ironwood:100:1,2:2").unwrap();
        drop(first);

        let retry = FaucetJournal::open(&dir).unwrap();
        assert_eq!(
            retry.key_for("wallet:ironwood:100:1,2:1").unwrap(),
            account_one
        );
        assert_eq!(
            retry.key_for("wallet:ironwood:100:1,2:2").unwrap(),
            account_two
        );
        retry
            .clear(&["wallet:ironwood:100:1,2:1", "wallet:ironwood:100:1,2:2"])
            .unwrap();
        drop(retry);

        let next = FaucetJournal::open(&dir).unwrap();
        assert_ne!(
            next.key_for("wallet:ironwood:100:1,2:1").unwrap(),
            account_one
        );
        drop(next);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn overlapping_faucet_process_rejects_the_locked_journal() {
        let dir = tempfile::tempdir().unwrap();
        let journal = FaucetJournal::open(dir.path()).unwrap();
        let mut retry = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runtime::tests::faucet_journal_lock_probe"])
            .env("THS_FAUCET_JOURNAL_LOCK_PROBE", dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let completed_while_locked = loop {
            if retry.try_wait().unwrap().is_some() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(journal);
        let output = retry.wait_with_output().unwrap();
        let result = String::from_utf8_lossy(&output.stdout);
        assert!(
            completed_while_locked && output.status.success() && result.contains("running 1 test"),
            "overlapping command waited or acquired the lock: {result}"
        );
    }

    #[test]
    fn faucet_journal_lock_probe() {
        let Some(dir) = std::env::var_os("THS_FAUCET_JOURNAL_LOCK_PROBE") else {
            return;
        };
        let error = FaucetJournal::open(std::path::Path::new(&dir))
            .err()
            .expect("overlapping command acquired the lock");
        assert!(
            error
                .to_string()
                .contains("another faucet command is running")
        );
    }

    #[test]
    fn unfinished_wallet_batch_rejects_an_overlapping_subset() {
        let dir = std::env::temp_dir().join(format!("ths-faucet-batch-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let journal = FaucetJournal::open(&dir).unwrap();
        let batch = journal.wallet_batch("ironwood", 100, &[1, 2]).unwrap();
        journal.key_for(&format!("{batch}:1")).unwrap();
        assert!(journal.wallet_batch("ironwood", 100, &[2]).is_err());
        journal.key_for(&format!("{batch}:2")).unwrap();

        assert_eq!(
            journal.wallet_batch("ironwood", 100, &[2, 1]).unwrap(),
            batch
        );
        assert!(journal.wallet_batch("ironwood", 100, &[2]).is_err());
        journal
            .clear(&[&format!("{batch}:1"), &format!("{batch}:2")])
            .unwrap();
        assert!(journal.wallet_batch("ironwood", 100, &[2]).is_ok());
        drop(journal);
        fs::remove_dir_all(dir).unwrap();
    }

    /// answers loopback requests in turn with canned statuses and bodies, passing on each
    /// request body.
    fn serve(responses: Vec<(&'static str, &'static str)>) -> (String, mpsc::Receiver<String>) {
        use std::io::{BufRead, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (requests, received) = mpsc::channel();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut request = vec![0; length];
                reader.read_exact(&mut request).unwrap();
                requests.send(String::from_utf8(request).unwrap()).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (url, received)
    }

    #[test]
    fn responses_decode_or_report_their_status_and_body() {
        let (url, _requests) = serve(vec![
            ("200 OK", r#"{"blocks":1,"hashes":["tip"]}"#),
            ("409 Conflict", "already claimed"),
            ("200 OK", "not json"),
        ]);
        let client = reqwest::blocking::Client::new();
        let get = || client.get(&url).send().unwrap();

        let mined: MineResult = decode_response(get()).unwrap();
        assert_eq!(mined.hashes, ["tip"]);
        let rejected = decode_response::<MineResult>(get())
            .unwrap_err()
            .to_string();
        assert_eq!(rejected, "rejected (409 Conflict): already claimed");
        let malformed = format!("{:#}", decode_response::<MineResult>(get()).unwrap_err());
        assert!(malformed.starts_with("decoding response: "), "{malformed}");
    }

    #[test]
    fn wallet_faucet_keeps_funding_after_a_failed_account_and_keeps_its_keys() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        let alpha = name("alpha");
        fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
        let pending = r#"{"id":"a","kind":"faucet","from_account":null,"to_account":2,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":100,"txid":"t","block_hash":null,"status":"broadcast"}"#;
        let funded = r#"{"id":"b","kind":"faucet","from_account":null,"to_account":3,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":100,"txid":"u","block_hash":"b","status":"confirmed"}"#;
        let (url, requests) = serve(vec![
            ("422 Unprocessable Entity", "no funds"),
            ("200 OK", pending),
            ("200 OK", funded),
        ]);

        let error = runtime
            .wallet_faucet_with(
                &alpha,
                &url,
                &reqwest::blocking::Client::new(),
                &[1, 2, 3],
                100,
                "ironwood",
                true,
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "2 of 3 faucet requests are pending or failed"
        );

        let requests: Vec<serde_json::Value> = requests
            .try_iter()
            .map(|request| serde_json::from_str(&request).unwrap())
            .collect();
        assert_eq!(requests.len(), 3);
        let journal = FaucetJournal::open(&runtime.instance_dir(&alpha)).unwrap();
        for (account, request) in (1..=3).zip(&requests) {
            assert_eq!(request["account_id"], account);
            // the unfinished batch keeps every key, so a retry submits the same ones.
            assert_eq!(
                request["idempotency_key"],
                journal
                    .key_for(&format!("wallet:ironwood:100:1,2,3:{account}"))
                    .unwrap()
            );
        }
    }

    #[test]
    fn readiness_failure_deletes_the_started_instance() {
        let (mut host, events) = RecordingHost::new();
        host.wait_ready_result = Err("dashboard did not become healthy".into());
        let (_sender, shutdown) = Shutdown::channel();
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
            .unwrap_err();
        assert!(err.to_string().contains("dashboard did not become healthy"));
        let events = events.lock().unwrap().clone();
        let allocate_pos = events
            .iter()
            .position(|e| e == "allocate:alpha")
            .expect("allocate:alpha");
        assert!(
            events[allocate_pos + 1..]
                .iter()
                .any(|e| e == "delete:alpha"),
            "expected delete:alpha after allocate:alpha, got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| e.starts_with("delete:") && !e.ends_with("alpha"))
        );
        assert!(!events.iter().any(|e| e.starts_with("wait_for_shutdown")));
    }

    #[test]
    fn browser_open_failure_deletes_and_does_not_wait() {
        let (mut host, events) = RecordingHost::new();
        host.open_url_result = Err("opening http://127.0.0.1:1".into());
        let (_sender, shutdown) = Shutdown::channel();
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
            .unwrap_err();
        assert!(err.to_string().contains("opening"));
        let events = events.lock().unwrap().clone();
        assert!(events.iter().any(|e| e.starts_with("open_url:")));
        assert!(events.iter().any(|e| e == "delete:alpha"));
        assert!(!events.iter().any(|e| e.starts_with("wait_for_shutdown")));
    }

    #[test]
    fn no_open_skips_browser_and_waits_for_shutdown() {
        let (host, events) = RecordingHost::new();
        let (sender, shutdown) = Shutdown::channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            sender.send(Ended::Interrupted).unwrap();
        });
        runtime_for_tests()
            .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
            .unwrap();
        let events = events.lock().unwrap().clone();
        assert!(!events.iter().any(|e| e.starts_with("open_url:")));
        assert!(events.iter().any(|e| e.starts_with("wait_for_shutdown")));
        assert!(events.iter().any(|e| e == "delete:alpha"));
    }

    #[test]
    fn deletion_elsewhere_ends_start_without_deleting_again() {
        let (mut host, events) = RecordingHost::new();
        host.deleted_elsewhere = true;
        let (_sender, shutdown) = Shutdown::channel();
        runtime_for_tests()
            .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
            .unwrap();
        let events = events.lock().unwrap().clone();
        let waited = events
            .iter()
            .position(|e| e == "wait_for_shutdown:ths-alpha-app")
            .expect("waited on the app container");
        assert!(
            !events[waited..].iter().any(|e| e.starts_with("delete:")),
            "deleted again after another command deleted it: {events:?}"
        );
    }

    #[test]
    fn interrupt_before_ready_deletes_the_started_instance() {
        let (mut host, events) = RecordingHost::new();
        host.interrupt_before_ready = true;
        let (_sender, shutdown) = Shutdown::channel();
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
            .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        let events = events.lock().unwrap().clone();
        assert!(events.iter().any(|e| e == "allocate:alpha"));
        assert!(events.iter().any(|e| e == "delete:alpha"));
        assert!(!events.iter().any(|e| e.starts_with("wait_for_shutdown")));
    }

    #[test]
    fn interrupt_during_allocate_deletes_only_the_named_instance() {
        let (host, events) = RecordingHost::new();
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::Interrupted).unwrap();
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
            .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        let events = events.lock().unwrap().clone();
        assert!(events.iter().any(|e| e == "delete:alpha"));
        assert!(!events.iter().any(|e| e.contains("beta")));
    }

    #[test]
    fn validates_instance_names() {
        for valid in ["default", "project-2", "a"] {
            assert!(valid.parse::<InstanceName>().is_ok());
        }
        for invalid in ["", "UPPER", "with space", "-start", "end-"] {
            assert!(invalid.parse::<InstanceName>().is_err());
        }
    }

    #[test]
    fn project_images_are_version_locked() {
        assert_eq!(
            app_image(),
            format!(
                "ghcr.io/zcashlabs/thus-spoke-zakura-app:{}",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert_eq!(
            lightwalletd_image(),
            format!(
                "ghcr.io/zcashlabs/thus-spoke-zakura-lightwalletd:{}",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn shutdown_reports_interrupt_from_channel() {
        let (sender, shutdown) = Shutdown::channel();
        assert!(!shutdown.try_interrupted());
        sender.send(Ended::Interrupted).unwrap();
        assert!(shutdown.try_interrupted());
        assert!(shutdown.try_interrupted());
        assert_eq!(shutdown.wait().unwrap(), Ended::Interrupted);
    }

    #[test]
    fn shutdown_check_bails_when_latched() {
        let (sender, shutdown) = Shutdown::channel();
        shutdown.check().unwrap();
        sender.send(Ended::Interrupted).unwrap();
        let err = shutdown.check().unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        let err = shutdown.check().unwrap_err();
        assert!(err.to_string().contains("interrupted"));
    }

    #[test]
    fn shutdown_wait_timeout_wakes_on_signal() {
        let (sender, shutdown) = Shutdown::channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            sender.send(Ended::Interrupted).unwrap();
        });
        let started = Instant::now();
        let err = shutdown.wait_timeout(Duration::from_secs(2)).unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn shutdown_wait_timeout_returns_on_idle() {
        let (_sender, shutdown) = Shutdown::channel();
        shutdown.wait_timeout(Duration::from_millis(20)).unwrap();
    }

    #[test]
    fn shutdown_wait_returns_after_signal() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::Interrupted).unwrap();
        assert_eq!(shutdown.wait().unwrap(), Ended::Interrupted);
    }

    #[test]
    fn shutdown_wait_returns_when_the_environment_is_gone() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::DeletedElsewhere).unwrap();
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert!(!shutdown.try_interrupted());
    }

    #[test]
    fn shutdown_wait_keeps_a_deletion_notice_that_arrived_first() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::DeletedElsewhere).unwrap();
        sender.send(Ended::Interrupted).unwrap();
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert!(shutdown.try_interrupted());
    }

    fn watched(id: &str) -> WatchedContainer {
        WatchedContainer {
            id: id.to_owned(),
            created: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    /// Scripted Docker answers for `DeletionMonitor`. An unscripted `identify` or `exists`
    /// fails like an unreachable daemon; an unscripted `wait_destroyed` blocks like a quiet
    /// event stream until cancelled.
    #[derive(Default)]
    struct ScriptedEvents {
        identify: Mutex<VecDeque<Result<Option<WatchedContainer>, String>>>,
        streams: Mutex<VecDeque<Result<bool, String>>>,
        exists: Mutex<VecDeque<Result<bool, String>>>,
        calls: Mutex<Vec<String>>,
        cancelled: AtomicBool,
    }

    impl ScriptedEvents {
        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn wait_for_calls(&self, count: usize) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.calls.lock().unwrap().len() < count {
                assert!(
                    Instant::now() < deadline,
                    "calls so far: {:?}",
                    self.calls()
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn scripted<T>(queue: &Mutex<VecDeque<Result<T, String>>>) -> Option<Result<T>> {
        queue
            .lock()
            .unwrap()
            .pop_front()
            .map(|answer| answer.map_err(|e| anyhow!("{e}")))
    }

    impl ContainerEvents for ScriptedEvents {
        fn identify(&self, name: &str) -> Result<Option<WatchedContainer>> {
            self.record(format!("identify:{name}"));
            scripted(&self.identify).unwrap_or_else(|| Err(anyhow!("Docker is unreachable")))
        }

        fn exists(&self, id: &str) -> Result<bool> {
            self.record(format!("exists:{id}"));
            scripted(&self.exists).unwrap_or_else(|| Err(anyhow!("Docker is unreachable")))
        }

        fn wait_destroyed(&self, container: &WatchedContainer) -> Result<bool> {
            self.record(format!("wait_destroyed:{}", container.id));
            if let Some(answer) = scripted(&self.streams) {
                return answer;
            }
            while !self.cancelled.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(false)
        }

        fn cancel(&self) {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }

    fn monitor(events: &Arc<ScriptedEvents>, shutdown: &Shutdown) -> DeletionMonitor {
        DeletionMonitor::spawn(
            events.clone(),
            "ths-alpha-app",
            Duration::from_millis(1),
            shutdown,
        )
    }

    #[test]
    fn deletion_monitor_reports_a_destroy_event() {
        let events = Arc::new(ScriptedEvents::default());
        events
            .identify
            .lock()
            .unwrap()
            .push_back(Ok(Some(watched("app-1"))));
        events.streams.lock().unwrap().push_back(Ok(true));
        let (_sender, shutdown) = Shutdown::channel();
        let _monitor = monitor(&events, &shutdown);
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert_eq!(
            events.calls(),
            ["identify:ths-alpha-app", "wait_destroyed:app-1"]
        );
    }

    #[test]
    fn deletion_monitor_reports_a_container_already_gone() {
        let events = Arc::new(ScriptedEvents::default());
        events.identify.lock().unwrap().push_back(Ok(None));
        let (_sender, shutdown) = Shutdown::channel();
        let _monitor = monitor(&events, &shutdown);
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
    }

    #[test]
    fn deletion_monitor_treats_docker_errors_as_unknown() {
        let events = Arc::new(ScriptedEvents::default());
        events.identify.lock().unwrap().extend([
            Err("Docker is unreachable".to_owned()),
            Ok(Some(watched("app-1"))),
        ]);
        events
            .streams
            .lock()
            .unwrap()
            .extend([Err("Docker is unreachable".to_owned()), Ok(false)]);
        let (sender, shutdown) = Shutdown::channel();
        let monitor = monitor(&events, &shutdown);
        // Both reconciliations fail like an unreachable daemon; the third stream stays open.
        events.wait_for_calls(7);
        sender.send(Ended::Interrupted).unwrap();
        assert_eq!(shutdown.wait().unwrap(), Ended::Interrupted);
        drop(monitor);
        assert_eq!(
            events.calls(),
            [
                "identify:ths-alpha-app",
                "identify:ths-alpha-app",
                "wait_destroyed:app-1",
                "exists:app-1",
                "wait_destroyed:app-1",
                "exists:app-1",
                "wait_destroyed:app-1",
            ]
        );
        assert!(shutdown.receiver.try_recv().is_err(), "reported a deletion");
    }

    #[test]
    fn deletion_monitor_resubscribes_after_a_disconnect() {
        let events = Arc::new(ScriptedEvents::default());
        events
            .identify
            .lock()
            .unwrap()
            .push_back(Ok(Some(watched("app-1"))));
        events.streams.lock().unwrap().extend([Ok(false), Ok(true)]);
        events.exists.lock().unwrap().push_back(Ok(true));
        let (_sender, shutdown) = Shutdown::channel();
        let _monitor = monitor(&events, &shutdown);
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert_eq!(
            events.calls(),
            [
                "identify:ths-alpha-app",
                "wait_destroyed:app-1",
                "exists:app-1",
                "wait_destroyed:app-1",
            ]
        );
    }

    #[test]
    fn deletion_monitor_follows_the_original_container_not_its_name() {
        // A disconnect hides the destroy event, and a new container already took the name.
        let events = Arc::new(ScriptedEvents::default());
        events
            .identify
            .lock()
            .unwrap()
            .extend([Ok(Some(watched("app-1"))), Ok(Some(watched("app-2")))]);
        events.streams.lock().unwrap().push_back(Ok(false));
        events.exists.lock().unwrap().push_back(Ok(false));
        let (_sender, shutdown) = Shutdown::channel();
        let _monitor = monitor(&events, &shutdown);
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert_eq!(
            events.calls(),
            [
                "identify:ths-alpha-app",
                "wait_destroyed:app-1",
                "exists:app-1",
            ]
        );
    }

    #[test]
    fn dropping_the_deletion_monitor_stops_the_watch() {
        let events = Arc::new(ScriptedEvents::default());
        events
            .identify
            .lock()
            .unwrap()
            .push_back(Ok(Some(watched("app-1"))));
        let (_sender, shutdown) = Shutdown::channel();
        let monitor = monitor(&events, &shutdown);
        events.wait_for_calls(2);
        drop(monitor);
        assert!(events.cancelled.load(Ordering::SeqCst));
        assert!(shutdown.receiver.try_recv().is_err(), "reported a deletion");
    }

    #[test]
    fn wait_ready_aborts_when_shutdown_is_signaled() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::Interrupted).unwrap();
        let err = wait_ready(
            "http://127.0.0.1:1",
            "missing-app",
            Duration::from_secs(5),
            &shutdown,
        )
        .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
    }

    #[test]
    fn wait_for_zakura_tip_aborts_when_shutdown_is_signaled() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::Interrupted).unwrap();
        let err = wait_for_zakura_tip(
            "http://127.0.0.1:1",
            "missing-zakura",
            Duration::from_secs(5),
            &shutdown,
        )
        .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
    }

    #[test]
    fn default_offset_uses_stable_loopback_ports() {
        let ports = host_ports(0).unwrap();
        assert_eq!(ports.dashboard, 32805);
        assert_eq!(ports.rpc, 18232);
        assert_eq!(ports.p2p, 18233);
        assert_eq!(ports.lightwalletd, 9067);
        assert_eq!(loopback_publish(ports.rpc, 18232), "127.0.0.1:18232:18232");
        assert_eq!(
            loopback_publish(ports.dashboard, 8080),
            "127.0.0.1:32805:8080"
        );
    }

    #[test]
    fn port_offset_shifts_all_four_hosts_by_the_same_stride() {
        let base = host_ports(0).unwrap();
        let shifted = host_ports(10).unwrap();
        assert_eq!(shifted.dashboard, base.dashboard + 10);
        assert_eq!(shifted.rpc, base.rpc + 10);
        assert_eq!(shifted.p2p, base.p2p + 10);
        assert_eq!(shifted.lightwalletd, base.lightwalletd + 10);
    }

    #[test]
    fn port_offset_rejects_values_that_are_not_multiples_of_ten() {
        let err = host_ports(1).unwrap_err();
        assert!(err.to_string().contains('1'));
    }

    #[test]
    fn port_offset_accepts_the_largest_value_that_keeps_every_port_in_range() {
        let ports = host_ports(32730).unwrap();
        assert_eq!(ports.dashboard, 65535);
    }

    #[test]
    fn port_offset_rejects_values_that_overflow_a_host_port() {
        let err = host_ports(32740).unwrap_err();
        assert!(err.to_string().contains("overflow"));
    }

    #[test]
    fn invalid_port_offset_is_rejected_before_any_deletion_or_allocation() {
        let (mut host, events) = RecordingHost::new();
        host.initial_delete_error = true;
        let (_sender, shutdown) = Shutdown::channel();

        let error = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 32740, &host, &shutdown)
            .unwrap_err();

        assert!(error.to_string().contains("overflow"));
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn reports_the_conflicting_loopback_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let err = require_free_loopback(port).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains(&port.to_string()),
            "error should name port {port}, got {message}"
        );
    }

    #[test]
    fn endpoints_json_includes_regtest_and_plaintext_lightwalletd() {
        let json = serde_json::to_value(endpoints_for(&host_ports(0).unwrap())).unwrap();
        assert_eq!(json["dashboard"], "http://127.0.0.1:32805");
        assert_eq!(json["rpc"], "http://127.0.0.1:18232");
        assert_eq!(json["lightwalletd"], "http://127.0.0.1:9067");
        assert_eq!(json["p2p"], "127.0.0.1:18233");
        assert_eq!(json["network"], "regtest");
        assert_eq!(json["tls"], false);
    }

    #[test]
    fn endpoints_json_without_network_fields_still_deserializes() {
        let parsed: Endpoints = serde_json::from_str(
            r#"{"dashboard":"http://127.0.0.1:1","rpc":"http://127.0.0.1:2","lightwalletd":"http://127.0.0.1:3","p2p":"127.0.0.1:4"}"#,
        )
        .unwrap();
        assert_eq!(parsed.network, "regtest");
        assert!(!parsed.tls);
    }
}
