use std::{
    fmt::{self, Display},
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const APP_IMAGE_REPOSITORY: &str = "ghcr.io/zcashlabs/thus-spoke-zakura-app";
const ZAKURA_IMAGE: &str = "zakuracore/zakura:1.4.0";
const LIGHTWALLETD_IMAGE_REPOSITORY: &str = "ghcr.io/zcashlabs/thus-spoke-zakura-lightwalletd";

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Endpoints {
    pub dashboard: String,
    pub rpc: String,
    pub lightwalletd: String,
    pub p2p: String,
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
    block_hash: String,
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

    pub fn start(&self, name: &InstanceName, no_open: bool, json: bool) -> Result<()> {
        self.doctor(false)?;
        for image in [app_image(), lightwalletd_image(), ZAKURA_IMAGE.to_owned()] {
            require_image(&image)?;
        }
        let shutdown = Shutdown::install()?;
        self.start_with(name, no_open, json, &DockerHost, &shutdown)
    }

    pub fn status(&self, name: &InstanceName, json: bool) -> Result<()> {
        let endpoints = inspect_endpoints(&prefix(name))
            .or_else(|_| self.read_instance(name).map(|i| i.endpoints))?;
        let running = container_running(&format!("{}-app", prefix(name))).unwrap_or(false);
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"name": name.to_string(), "running": running, "endpoints": endpoints})
                )?
            );
        } else {
            println!("{}: {}", name, if running { "running" } else { "stopped" });
            print_endpoints(name, &endpoints);
        }
        Ok(())
    }

    pub fn endpoints(&self, name: &InstanceName, json: bool) -> Result<()> {
        let endpoints = self.read_instance(name)?.endpoints;
        if json {
            println!("{}", serde_json::to_string_pretty(&endpoints)?);
        } else {
            print_endpoints(name, &endpoints);
        }
        Ok(())
    }

    pub fn open(&self, name: &InstanceName) -> Result<()> {
        open_url(&self.read_instance(name)?.endpoints.dashboard)
    }

    pub fn mine(&self, name: &InstanceName, blocks: u32, json: bool) -> Result<()> {
        let app_container = format!("{}-app", prefix(name));
        if !container_running(&app_container).unwrap_or(false) {
            bail!("environment {name} is not running; start it with `ths --name {name}`");
        }
        let dashboard = self.read_instance(name)?.endpoints.dashboard;
        let response = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?
            .post(format!("{dashboard}/api/v1/mine"))
            .json(&serde_json::json!({"blocks": blocks}))
            .send()
            .with_context(|| format!("asking environment {name} to mine {blocks} blocks"))?;
        let status = response.status();
        if !status.is_success() {
            let detail = response
                .text()
                .unwrap_or_else(|_| "response body was unreadable".to_owned());
            bail!("environment {name} rejected mining ({status}): {detail}");
        }
        let result: MineResult = response.json().context("decoding mining response")?;
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
        let app_container = format!("{}-app", prefix(name));
        if !container_running(&app_container).unwrap_or(false) {
            bail!("environment {name} is not running; start it with `ths --name {name}`");
        }
        let dashboard = self.read_instance(name)?.endpoints.dashboard;
        let response = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?
            .post(format!("{dashboard}/api/v1/faucet/address"))
            .json(&serde_json::json!({
                "address": address,
                "amount_zatoshi": amount_zatoshi,
            }))
            .send()
            .with_context(|| format!("asking environment {name} to fund {address}"))?;
        let status = response.status();
        if !status.is_success() {
            let detail = response
                .text()
                .unwrap_or_else(|_| "response body was unreadable".to_owned());
            bail!("environment {name} rejected faucet request ({status}): {detail}");
        }
        let result: FaucetResult = response.json().context("decoding faucet response")?;
        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!(
                "Sent {} ZEC to {} on {name}.",
                format_zec(result.amount_zatoshi),
                result.address
            );
            println!("Transaction: {}", result.txid);
            println!("Confirmed in: {}", result.block_hash);
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
        let app_container = format!("{}-app", prefix(name));
        if !container_running(&app_container).unwrap_or(false) {
            bail!("environment {name} is not running; start it with `ths --name {name}`");
        }
        let dashboard = self.read_instance(name)?.endpoints.dashboard;
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        let mut funded = Vec::new();
        let mut failures = Vec::new();
        for &account_id in accounts {
            let idempotency_key =
                format!("ths-wallet-faucet-{account_id}-{}", uuid::Uuid::new_v4());
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
            match outcome.and_then(decode_activity) {
                Ok(activity) => funded.push(activity),
                Err(error) => failures.push(format!("account {account_id}: {error:#}")),
            }
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"funded": funded, "failed": failures})
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
            for failure in &failures {
                eprintln!("Failed to fund {failure}");
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            bail!(
                "{} of {} faucet requests failed",
                failures.len(),
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
        let app_container = format!("{}-app", prefix(name));
        if !container_running(&app_container).unwrap_or(false) {
            bail!("environment {name} is not running; start it with `ths --name {name}`");
        }
        let dashboard = self.read_instance(name)?.endpoints.dashboard;
        let idempotency_key = format!("ths-wallet-send-{}", uuid::Uuid::new_v4());
        let response = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?
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
            .with_context(|| {
                format!("asking environment {name} to send from account {from} to account {to}")
            })?;
        let activity = decode_activity(response)?;
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

    pub fn logs(&self, name: &InstanceName, service: Option<&str>, follow: bool) -> Result<()> {
        let service = service.unwrap_or("app");
        let mut args = vec!["logs"];
        if follow {
            args.push("--follow");
        }
        let container = format!("{}-{service}", prefix(name));
        args.push(&container);
        docker_inherit(&args)
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
        let prefix = prefix(name);
        let mut failures = Vec::new();
        for service in ["app", "lightwalletd", "zakura", "init"] {
            let target = format!("{prefix}-{service}");
            match container_exists(&target) {
                Ok(true) => {
                    if let Err(error) = docker(["rm", "-f", &target]) {
                        failures.push(format!("container {target}: {error}"));
                    }
                }
                Ok(false) => {}
                Err(error) => failures.push(format!("container {target}: {error}")),
            }
        }
        for suffix in ["chain", "wallet", "lightwalletd", "config"] {
            let volume = format!("{prefix}-{suffix}");
            if docker_output(["volume", "inspect", &volume]).is_ok()
                && let Err(error) = docker(["volume", "rm", &volume])
            {
                failures.push(format!("volume {volume}: {error}"));
            }
        }
        if docker_output(["network", "inspect", &prefix]).is_ok()
            && let Err(error) = docker(["network", "rm", &prefix])
        {
            failures.push(format!("network {prefix}: {error}"));
        }
        let dir = self.instance_dir(name);
        if dir.exists()
            && let Err(error) = fs::remove_dir_all(&dir)
        {
            failures.push(format!("metadata {}: {error}", dir.display()));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            bail!(
                "could not delete every instance resource: {}",
                failures.join("; ")
            )
        }
    }
}

fn decode_activity(response: reqwest::blocking::Response) -> Result<Activity> {
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

fn prefix(name: &InstanceName) -> String {
    format!("tsz-{name}")
}
fn label(name: &InstanceName) -> String {
    format!("com.zakura.tsz.instance={name}")
}

fn ensure_network(prefix: &str) -> Result<()> {
    if docker_output(["network", "inspect", prefix]).is_err() {
        docker(["network", "create", prefix])?;
    }
    Ok(())
}
fn ensure_volume(volume: &str, name: &InstanceName) -> Result<()> {
    if docker_output(["volume", "inspect", volume]).is_err() {
        docker(["volume", "create", "--label", &label(name), volume])?;
    }
    Ok(())
}
fn ensure_zakura(prefix: &str, name: &InstanceName) -> Result<()> {
    let target = format!("{prefix}-zakura");
    if !container_exists(&target)? {
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
            "127.0.0.1::18232",
            "-p",
            "127.0.0.1::18233",
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
fn ensure_lightwalletd(prefix: &str, name: &InstanceName) -> Result<()> {
    let target = format!("{prefix}-lightwalletd");
    if !container_exists(&target)? {
        let image = lightwalletd_image();
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
            "127.0.0.1::9067",
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
fn ensure_app(prefix: &str, name: &InstanceName) -> Result<()> {
    let target = format!("{prefix}-app");
    if !container_exists(&target)? {
        let public_rpc = format!(
            "http://127.0.0.1:{}",
            published_port(&format!("{prefix}-zakura"), "18232/tcp")?
        );
        let public_lightwalletd = format!(
            "http://127.0.0.1:{}",
            published_port(&format!("{prefix}-lightwalletd"), "9067/tcp")?
        );
        let public_p2p = format!(
            "127.0.0.1:{}",
            published_port(&format!("{prefix}-zakura"), "18233/tcp")?
        );
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
            "127.0.0.1::8080",
            "-e",
            "TSZ_LISTEN=0.0.0.0:8080",
            "-e",
            "TSZ_ZAKURA_RPC=http://zakura:18232",
            "-e",
            "TSZ_LIGHTWALLETD=http://lightwalletd:9067",
            "-e",
            &format!("TSZ_INSTANCE={name}"),
            "-e",
            &format!("TSZ_PUBLIC_ZAKURA_RPC={public_rpc}"),
            "-e",
            &format!("TSZ_PUBLIC_LIGHTWALLETD={public_lightwalletd}"),
            "-e",
            &format!("TSZ_PUBLIC_P2P={public_p2p}"),
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
    Ok(docker_output(["container", "inspect", name]).is_ok())
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
    receiver: mpsc::Receiver<()>,
}

impl Shutdown {
    #[cfg(test)]
    fn from_receiver(receiver: mpsc::Receiver<()>) -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            receiver,
        }
    }

    fn install() -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let flag = Arc::new(AtomicBool::new(false));
        let handler_flag = flag.clone();
        ctrlc::set_handler(move || {
            handler_flag.store(true, Ordering::SeqCst);
            let _ = sender.send(());
        })
        .context("installing the shutdown signal handler")?;
        Ok(Self { flag, receiver })
    }

    fn try_interrupted(&self) -> bool {
        if self.receiver.try_recv().is_ok() {
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

    fn wait(&self) -> Result<()> {
        if self.try_interrupted() {
            return Ok(());
        }
        self.receiver
            .recv()
            .context("waiting for a shutdown signal")?;
        self.flag.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn wait_timeout(&self, timeout: Duration) -> Result<()> {
        if self.try_interrupted() {
            bail!("interrupted");
        }
        match self.receiver.recv_timeout(timeout) {
            Ok(()) => {
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

trait StartHost {
    fn delete(&self, runtime: &Runtime, name: &InstanceName) -> Result<()>;
    fn allocate(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
    ) -> Result<Endpoints>;
    fn wait_ready(
        &self,
        endpoints: &Endpoints,
        app_container: &str,
        timeout: Duration,
        shutdown: &Shutdown,
    ) -> Result<()>;
    fn open_url(&self, url: &str) -> Result<()>;
    fn wait_for_shutdown(&self, shutdown: &Shutdown) -> Result<()>;
}

struct DockerHost;

impl StartHost for DockerHost {
    fn delete(&self, runtime: &Runtime, name: &InstanceName) -> Result<()> {
        runtime.delete_instance_resources(name)
    }

    fn allocate(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
    ) -> Result<Endpoints> {
        fs::create_dir_all(runtime.instance_dir(name))?;
        let prefix = prefix(name);
        ensure_network(&prefix)?;
        shutdown.check()?;
        for suffix in ["chain", "wallet", "lightwalletd", "config"] {
            ensure_volume(&format!("{prefix}-{suffix}"), name)?;
        }
        shutdown.check()?;

        if !container_exists(&format!("{prefix}-init"))? {
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

        ensure_zakura(&prefix, name)?;
        shutdown.check()?;
        ensure_lightwalletd(&prefix, name)?;
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
        ensure_app(&prefix, name)?;
        shutdown.check()?;
        docker(["start", &format!("{prefix}-app")])?;
        shutdown.check()?;
        let endpoints = inspect_endpoints(&prefix)?;
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

    fn wait_for_shutdown(&self, shutdown: &Shutdown) -> Result<()> {
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
        if let Err(error) = self.host.delete(self.runtime, self.name) {
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
        host: &dyn StartHost,
        shutdown: &Shutdown,
    ) -> Result<()> {
        let mut cleanup = CleanupOnDrop {
            runtime: self,
            name,
            host,
            active: true,
        };
        println!("Preparing a fresh {name} environment…");
        host.delete(self, name)?;
        println!("Starting {name}…");
        let endpoints = host.allocate(self, name, shutdown)?;
        shutdown.check()?;
        host.wait_ready(
            &endpoints,
            &format!("{}-app", prefix(name)),
            Duration::from_secs(120),
            shutdown,
        )?;
        if json {
            println!("{}", serde_json::to_string_pretty(&endpoints)?);
        } else {
            print_endpoints(name, &endpoints);
        }
        if !no_open {
            host.open_url(&endpoints.dashboard)?;
        }
        if !json {
            println!("\nPress Ctrl+C to stop and delete this development environment.");
        }
        host.wait_for_shutdown(shutdown)?;
        println!("\nStopping and deleting {name}…");
        host.delete(self, name)?;
        cleanup.active = false;
        println!("Deleted {name} and all of its development data.");
        Ok(())
    }
}

fn container_running(name: &str) -> Result<bool> {
    Ok(docker_output([
        "container",
        "inspect",
        "--format",
        "{{.State.Running}}",
        name,
    ])? == "true")
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
        if !container_running(app_container).unwrap_or(false) {
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
        if !container_running(container).unwrap_or(false) {
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
fn print_endpoints(name: &InstanceName, e: &Endpoints) {
    println!(
        "\n{name} is ready 🌸\n  Dashboard    {}\n  Zakura RPC   {}\n  lightwalletd {}\n  P2P          {}",
        e.dashboard, e.rpc, e.lightwalletd, e.p2p
    );
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
fn docker<const N: usize>(args: [&str; N]) -> Result<()> {
    docker_inherit(&args)
}
fn docker_inherit(args: &[&str]) -> Result<()> {
    docker_command(args, None)
}
fn docker_inherit_in(args: &[&str], current_dir: &std::path::Path) -> Result<()> {
    docker_command(args, Some(current_dir))
}
fn docker_command(args: &[&str], current_dir: Option<&std::path::Path>) -> Result<()> {
    let mut command = Command::new("docker");
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
    let output = Command::new("docker")
        .args(args)
        .output()
        .context("running Docker")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
fn docker_logs(container: &str) -> Result<String> {
    let output = Command::new("docker")
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
    use std::sync::{Arc, Mutex};

    struct RecordingHost {
        events: Arc<Mutex<Vec<String>>>,
        wait_ready_result: Result<(), String>,
        open_url_result: Result<(), String>,
        interrupt_before_ready: bool,
    }

    impl RecordingHost {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    events: events.clone(),
                    wait_ready_result: Ok(()),
                    open_url_result: Ok(()),
                    interrupt_before_ready: false,
                },
                events,
            )
        }

        fn push(&self, event: &str) {
            self.events.lock().unwrap().push(event.to_owned());
        }
    }

    impl StartHost for RecordingHost {
        fn delete(&self, _runtime: &Runtime, name: &InstanceName) -> Result<()> {
            self.push(&format!("delete:{name}"));
            Ok(())
        }

        fn allocate(
            &self,
            _runtime: &Runtime,
            name: &InstanceName,
            shutdown: &Shutdown,
        ) -> Result<Endpoints> {
            self.push(&format!("allocate:{name}"));
            shutdown.check()?;
            Ok(Endpoints {
                dashboard: "http://127.0.0.1:1".into(),
                rpc: "http://127.0.0.1:2".into(),
                lightwalletd: "http://127.0.0.1:3".into(),
                p2p: "127.0.0.1:4".into(),
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

        fn wait_for_shutdown(&self, shutdown: &Shutdown) -> Result<()> {
            self.push("wait_for_shutdown");
            shutdown.wait()
        }
    }

    fn runtime_for_tests() -> Runtime {
        Runtime {
            root: std::env::temp_dir().join("tsz-start-cleanup-tests"),
        }
    }

    fn name(value: &str) -> InstanceName {
        value.parse().unwrap()
    }

    #[test]
    fn readiness_failure_deletes_the_started_instance() {
        let (mut host, events) = RecordingHost::new();
        host.wait_ready_result = Err("dashboard did not become healthy".into());
        let (_sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, &host, &shutdown)
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
        assert!(!events.iter().any(|e| e == "wait_for_shutdown"));
    }

    #[test]
    fn browser_open_failure_deletes_and_does_not_wait() {
        let (mut host, events) = RecordingHost::new();
        host.open_url_result = Err("opening http://127.0.0.1:1".into());
        let (_sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, &host, &shutdown)
            .unwrap_err();
        assert!(err.to_string().contains("opening"));
        let events = events.lock().unwrap().clone();
        assert!(events.iter().any(|e| e.starts_with("open_url:")));
        assert!(events.iter().any(|e| e == "delete:alpha"));
        assert!(!events.iter().any(|e| e == "wait_for_shutdown"));
    }

    #[test]
    fn no_open_skips_browser_and_waits_for_shutdown() {
        let (host, events) = RecordingHost::new();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            sender.send(()).unwrap();
        });
        let shutdown = Shutdown::from_receiver(receiver);
        runtime_for_tests()
            .start_with(&name("alpha"), true, false, &host, &shutdown)
            .unwrap();
        let events = events.lock().unwrap().clone();
        assert!(!events.iter().any(|e| e.starts_with("open_url:")));
        assert!(events.iter().any(|e| e == "wait_for_shutdown"));
        assert!(events.iter().any(|e| e == "delete:alpha"));
    }

    #[test]
    fn interrupt_before_ready_deletes_the_started_instance() {
        let (mut host, events) = RecordingHost::new();
        host.interrupt_before_ready = true;
        let (_sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, &host, &shutdown)
            .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        let events = events.lock().unwrap().clone();
        assert!(events.iter().any(|e| e == "allocate:alpha"));
        assert!(events.iter().any(|e| e == "delete:alpha"));
        assert!(!events.iter().any(|e| e == "wait_for_shutdown"));
    }

    #[test]
    fn interrupt_during_allocate_deletes_only_the_named_instance() {
        let (host, events) = RecordingHost::new();
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(()).unwrap();
        let shutdown = Shutdown::from_receiver(receiver);
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, &host, &shutdown)
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
        let (sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        assert!(!shutdown.try_interrupted());
        sender.send(()).unwrap();
        assert!(shutdown.try_interrupted());
        assert!(shutdown.try_interrupted());
        shutdown.wait().unwrap();
    }

    #[test]
    fn shutdown_check_bails_when_latched() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        shutdown.check().unwrap();
        sender.send(()).unwrap();
        let err = shutdown.check().unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        let err = shutdown.check().unwrap_err();
        assert!(err.to_string().contains("interrupted"));
    }

    #[test]
    fn shutdown_wait_timeout_wakes_on_signal() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            sender.send(()).unwrap();
        });
        let started = Instant::now();
        let err = shutdown.wait_timeout(Duration::from_secs(2)).unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn shutdown_wait_timeout_returns_on_idle() {
        let (_sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        shutdown.wait_timeout(Duration::from_millis(20)).unwrap();
    }

    #[test]
    fn shutdown_wait_returns_after_signal() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        sender.send(()).unwrap();
        shutdown.wait().unwrap();
    }

    #[test]
    fn wait_ready_aborts_when_shutdown_is_signaled() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        sender.send(()).unwrap();
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
        let (sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        sender.send(()).unwrap();
        let err = wait_for_zakura_tip(
            "http://127.0.0.1:1",
            "missing-zakura",
            Duration::from_secs(5),
            &shutdown,
        )
        .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
    }
}
