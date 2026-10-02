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
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const APP_IMAGE_REPOSITORY: &str = "ghcr.io/zcashlabs/thus-spoke-zakura-app";
const ZAKURA_IMAGE: &str = "zakuracore/zakura:1.6.0";
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
    /// unix seconds; missing for environments created before it was recorded.
    #[serde(default)]
    created_at: Option<u64>,
}

/// the containers every environment has. `init` is one-shot, so it exits once setup is done.
const SERVICES: [&str; 4] = ["app", "zakura", "lightwalletd", "init"];

/// one container's docker state (`running`, `exited`, ...) or `missing`, with its unix-second
/// creation and start times.
#[derive(Debug, Serialize)]
struct ContainerStatus {
    service: &'static str,
    state: String,
    created_at: Option<u64>,
    started_at: Option<u64>,
}

#[derive(Debug, Serialize)]
struct EnvironmentStatus {
    #[serde(flatten)]
    instance: Instance,
    /// `running`, `degraded` (some services running) or `stopped`.
    state: &'static str,
    containers: Vec<ContainerStatus>,
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

    pub fn start(
        &self,
        name: &InstanceName,
        no_open: bool,
        json: bool,
        port_offset: u16,
    ) -> Result<()> {
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
        let running = container_running(&format!("{}-app", prefix(name))).unwrap_or(false);
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
        let mut environments = Vec::new();
        if self.root.exists() {
            for entry in fs::read_dir(&self.root)? {
                let path = entry?.path().join("instance.json");
                if path.exists() {
                    let instance = serde_json::from_slice::<Instance>(&fs::read(&path)?)?;
                    let name = instance.name.parse()?;
                    let containers = SERVICES
                        .into_iter()
                        .map(|service| container_status(&name, service))
                        .collect::<Result<Vec<_>>>()?;
                    environments.push(EnvironmentStatus {
                        state: environment_state(&containers),
                        instance,
                        containers,
                    });
                }
            }
        }
        environments.sort_by(|left, right| left.instance.name.cmp(&right.instance.name));
        if json {
            println!("{}", serde_json::to_string_pretty(&environments)?);
        } else if environments.is_empty() {
            println!("No environments yet.");
        } else {
            print!("{}", render_list(&environments, now_unix()?));
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
            created_at: Some(now_unix()?),
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
        dashboard: 32805 + offset,
        rpc: 18232 + offset,
        p2p: 18233 + offset,
        lightwalletd: 9067 + offset,
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
    format!("com.zakura.ths.instance={name}")
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
fn ensure_zakura(prefix: &str, name: &InstanceName, ports: &HostPorts) -> Result<()> {
    let target = format!("{prefix}-zakura");
    if !container_exists(&target)? {
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
    if !container_exists(&target)? {
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
    if !container_exists(&target)? {
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
        port_offset: u16,
    ) -> Result<Endpoints> {
        fs::create_dir_all(runtime.instance_dir(name))?;
        let prefix = prefix(name);
        let ports = host_ports(port_offset)?;
        require_free_loopback(ports.dashboard)?;
        require_free_loopback(ports.rpc)?;
        require_free_loopback(ports.p2p)?;
        require_free_loopback(ports.lightwalletd)?;
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
        port_offset: u16,
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
        let endpoints = host.allocate(self, name, shutdown, port_offset)?;
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
            println!("\n{name} is ready 🌸\n{}", endpoint_lines(&endpoints));
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
/// an environment's state from its long-running services; `init` is not one of them.
fn environment_state(containers: &[ContainerStatus]) -> &'static str {
    let services = containers.iter().filter(|c| c.service != "init");
    let running = services.clone().filter(|c| c.state == "running").count();
    match running {
        0 => "stopped",
        n if n == services.count() => "running",
        _ => "degraded",
    }
}
fn container_status(name: &InstanceName, service: &'static str) -> Result<ContainerStatus> {
    let container = format!("{}-{service}", prefix(name));
    let output = match docker_output([
        "container",
        "inspect",
        "--format",
        "{{.State.Status}} {{.Created}} {{.State.StartedAt}}",
        &container,
    ]) {
        Ok(output) => output,
        Err(error) if error.to_string().contains("No such container") => {
            return Ok(ContainerStatus {
                service,
                state: "missing".into(),
                created_at: None,
                started_at: None,
            });
        }
        Err(error) => return Err(error),
    };
    let mut fields = output.split_whitespace();
    Ok(ContainerStatus {
        service,
        state: fields.next().unwrap_or("unknown").to_owned(),
        created_at: fields.next().and_then(parse_docker_time),
        started_at: fields.next().and_then(parse_docker_time),
    })
}
/// unix seconds from docker's utc rfc 3339 timestamps. docker reports a container that never
/// started as started in year one, which gives nothing.
fn parse_docker_time(value: &str) -> Option<u64> {
    let (date, time) = value.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let time = time.strip_suffix('Z')?;
    let mut time = time.split(['.', ':']).map(str::parse::<i64>);
    let (hour, minute, second) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if year < 1970 {
        return None;
    }
    // days since the epoch for a proleptic gregorian date.
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()
}
fn now_unix() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before 1970")?
        .as_secs())
}
fn ago(now: u64, then: u64) -> String {
    let seconds = now.saturating_sub(then);
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}
fn render_list(environments: &[EnvironmentStatus], now: u64) -> String {
    let (active, inactive): (Vec<_>, Vec<_>) =
        environments.iter().partition(|e| e.state != "stopped");
    let has_inactive = !inactive.is_empty();
    let mut out = String::new();
    for (title, section) in [("ACTIVE", active), ("INACTIVE", inactive)] {
        if section.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(title);
        out.push('\n');
        for e in section {
            let mut line = format!(
                "{:<16} {:<26} {:<8}",
                e.instance.name, e.instance.endpoints.dashboard, e.state
            );
            if let Some(created) = e.instance.created_at {
                line.push_str(&format!("   created {}", ago(now, created)));
            }
            out.push_str(line.trim_end());
            out.push('\n');
            for c in &e.containers {
                let mut line = format!("  {:<13} {:<9}", c.service, c.state);
                if let Some(created) = c.created_at {
                    line.push_str(&format!(" created {:<9}", ago(now, created)));
                }
                if let Some(started) = c.started_at {
                    line.push_str(&format!(" started {}", ago(now, started)));
                }
                out.push_str(line.trim_end());
                out.push('\n');
            }
        }
    }
    if has_inactive {
        out.push_str(
            "\nRemove an inactive environment and all of its data with:\n  ths --name <name> stop\n",
        );
    }
    out
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

    fn environment(name: &str, states: [&str; 4]) -> EnvironmentStatus {
        let containers = SERVICES
            .into_iter()
            .zip(states)
            .map(|(service, state)| ContainerStatus {
                service,
                state: state.into(),
                created_at: (state != "missing").then_some(1_000),
                started_at: (state == "running" || state == "exited").then_some(1_060),
            })
            .collect::<Vec<_>>();
        EnvironmentStatus {
            instance: Instance {
                name: name.into(),
                version: 1,
                endpoints: endpoints_for(&HostPorts {
                    dashboard: 8080,
                    rpc: 18232,
                    lightwalletd: 9067,
                    p2p: 18233,
                }),
                created_at: Some(1_000),
            },
            state: environment_state(&containers),
            containers,
        }
    }

    #[test]
    fn environments_are_classified_by_their_long_running_services() {
        let state = |states| environment("e", states).state;
        assert_eq!(
            state(["running", "running", "running", "exited"]),
            "running"
        );
        assert_eq!(
            state(["exited", "running", "running", "exited"]),
            "degraded"
        );
        assert_eq!(
            state(["missing", "running", "missing", "missing"]),
            "degraded"
        );
        assert_eq!(state(["exited", "exited", "exited", "exited"]), "stopped");
        assert_eq!(
            state(["missing", "missing", "missing", "missing"]),
            "stopped"
        );
        // init is one-shot: running or not, it does not decide the state.
        assert_eq!(state(["exited", "exited", "exited", "running"]), "stopped");
    }

    #[test]
    fn the_list_groups_active_before_inactive_and_hints_only_for_inactive() {
        let active = environment("default", ["running", "running", "running", "exited"]);
        let degraded = environment("partial", ["exited", "running", "running", "exited"]);
        let stopped = environment("old", ["missing", "missing", "missing", "missing"]);
        let rendered = render_list(&[stopped, active, degraded], 1_000 + 7_200);
        assert_eq!(
            rendered,
            concat!(
                "ACTIVE\n",
                "default          http://127.0.0.1:8080      running    created 2h ago\n",
                "  app           running   created 2h ago    started 1h ago\n",
                "  zakura        running   created 2h ago    started 1h ago\n",
                "  lightwalletd  running   created 2h ago    started 1h ago\n",
                "  init          exited    created 2h ago    started 1h ago\n",
                "partial          http://127.0.0.1:8080      degraded   created 2h ago\n",
                "  app           exited    created 2h ago    started 1h ago\n",
                "  zakura        running   created 2h ago    started 1h ago\n",
                "  lightwalletd  running   created 2h ago    started 1h ago\n",
                "  init          exited    created 2h ago    started 1h ago\n",
                "\n",
                "INACTIVE\n",
                "old              http://127.0.0.1:8080      stopped    created 2h ago\n",
                "  app           missing\n",
                "  zakura        missing\n",
                "  lightwalletd  missing\n",
                "  init          missing\n",
                "\n",
                "Remove an inactive environment and all of its data with:\n",
                "  ths --name <name> stop\n",
            )
        );
        // no inactive environment: no empty section and no hint.
        let only_active = render_list(
            &[environment(
                "default",
                ["running", "running", "running", "exited"],
            )],
            1_000,
        );
        assert!(!only_active.contains("INACTIVE") && !only_active.contains("ths --name"));
        let only_inactive = render_list(
            &[environment("old", ["exited", "exited", "exited", "exited"])],
            1_000,
        );
        assert!(only_inactive.starts_with("INACTIVE\n") && only_inactive.contains("ths --name"));
    }

    #[test]
    fn the_json_list_keeps_its_fields_and_adds_absolute_status() {
        let json = serde_json::to_value(environment(
            "default",
            ["running", "exited", "missing", "exited"],
        ))
        .unwrap();
        assert_eq!(json["name"], "default");
        assert_eq!(json["version"], 1);
        assert_eq!(json["endpoints"]["dashboard"], "http://127.0.0.1:8080");
        assert_eq!(json["state"], "degraded");
        assert_eq!(json["created_at"], 1_000);
        assert_eq!(json["containers"][0]["service"], "app");
        assert_eq!(json["containers"][0]["started_at"], 1_060);
        assert_eq!(json["containers"][2]["state"], "missing");
        assert!(json["containers"][2]["created_at"].is_null());
    }

    #[test]
    fn environments_recorded_before_creation_times_still_list() {
        let mut old = environment("old", ["missing", "missing", "missing", "missing"]);
        let mut legacy = serde_json::to_value(&old.instance).unwrap();
        legacy.as_object_mut().unwrap().remove("created_at");
        old.instance = serde_json::from_value(legacy).unwrap();
        assert_eq!(old.instance.created_at, None);
        assert!(
            render_list(&[old], 1_000)
                .starts_with("INACTIVE\nold              http://127.0.0.1:8080      stopped\n")
        );
    }

    #[test]
    fn docker_timestamps_parse_to_unix_seconds() {
        assert_eq!(parse_docker_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_docker_time("2026-10-02T21:58:53.061174304Z"),
            Some(1_790_978_333)
        );
        assert_eq!(
            parse_docker_time("2024-02-29T12:00:00Z"),
            Some(1_709_208_000)
        );
        // docker's zero time for a container that never started.
        assert_eq!(parse_docker_time("0001-01-01T00:00:00Z"), None);
        assert_eq!(parse_docker_time("not a time"), None);
    }

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

        fn wait_for_shutdown(&self, shutdown: &Shutdown) -> Result<()> {
            self.push("wait_for_shutdown");
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
    fn readiness_failure_deletes_the_started_instance() {
        let (mut host, events) = RecordingHost::new();
        host.wait_ready_result = Err("dashboard did not become healthy".into());
        let (_sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
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
        assert!(!events.iter().any(|e| e == "wait_for_shutdown"));
    }

    #[test]
    fn browser_open_failure_deletes_and_does_not_wait() {
        let (mut host, events) = RecordingHost::new();
        host.open_url_result = Err("opening http://127.0.0.1:1".into());
        let (_sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
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
            .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
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
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
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
