use std::{
    ffi::OsString,
    fmt::{self, Display},
    fs::{self, File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
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
    #[serde(default)]
    generation: uuid::Uuid,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RuntimeStatus {
    node_height: Option<u64>,
    lightwalletd_height: Option<u64>,
    wallet_scanned_height: Option<u64>,
    wallet_sync_state: String,
    settled: bool,
    settlement_blocker: Option<String>,
}

pub struct Runtime {
    root: PathBuf,
}

#[derive(Debug)]
struct InstanceLock {
    _file: File,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
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

    pub fn start(&self, name: &InstanceName, no_open: bool, json: bool) -> Result<()> {
        self.doctor(false)?;
        for image in [app_image(), lightwalletd_image(), ZAKURA_IMAGE.to_owned()] {
            require_image(&image)?;
        }
        let shutdown = Shutdown::install()?;
        self.start_with(name, no_open, json, &DockerHost, &shutdown)
    }

    pub fn wait(&self, name: &InstanceName, timeout: Duration, json: bool) -> Result<()> {
        let shutdown = Shutdown::install()?;
        let endpoints = self.read_instance(name)?.endpoints;
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("settlement timeout is too large")?;
        let status = wait_until_settled(
            &DockerHost,
            &endpoints,
            &format!("{}-app", prefix(name)),
            deadline,
            Duration::from_millis(750),
            &shutdown,
            !json,
        )?;
        if json {
            println!("{}", serde_json::to_string_pretty(&status)?);
        } else {
            println!(
                "{name} is settled at node {}, lightwalletd {}, wallet {}.",
                display_height(status.node_height),
                display_height(status.lightwalletd_height),
                display_height(status.wallet_scanned_height)
            );
        }
        Ok(())
    }

    pub fn run(&self, name: &InstanceName, timeout: Duration, command: &[OsString]) -> Result<u8> {
        self.doctor(false)?;
        for image in [app_image(), lightwalletd_image(), ZAKURA_IMAGE.to_owned()] {
            require_image(&image)?;
        }
        let shutdown = Shutdown::install()?;
        self.run_with(
            name,
            timeout,
            command,
            &std::env::current_dir().context("reading the current directory")?,
            &DockerHost,
            &shutdown,
        )
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
        let _lock = self.lock_instance(name)?;
        self.delete_instance_resources(name)?;
        println!("Stopped and deleted {name} and all of its development data.");
        Ok(())
    }

    pub fn reset(&self, name: &InstanceName, force: bool) -> Result<()> {
        if !force {
            bail!("reset deletes chain, wallet, and seed data; repeat with --force");
        }
        let _lock = self.lock_instance(name)?;
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

    fn open_instance_lock(&self, name: &InstanceName) -> Result<File> {
        let lock_dir = self.root.join(".locks");
        fs::create_dir_all(&lock_dir)
            .with_context(|| format!("creating lifecycle lock directory {}", lock_dir.display()))?;
        let path = lock_dir.join(format!("{name}.lock"));
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening lifecycle lock {}", path.display()))
    }

    fn lock_instance(&self, name: &InstanceName) -> Result<InstanceLock> {
        let file = self.open_instance_lock(name)?;
        match file.try_lock() {
            Ok(()) => Ok(InstanceLock { _file: file }),
            Err(TryLockError::WouldBlock) => {
                bail!("environment {name} is already in use by another command")
            }
            Err(TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("locking environment {name}"))
            }
        }
    }

    fn lock_instance_blocking(&self, name: &InstanceName) -> Result<InstanceLock> {
        let file = self.open_instance_lock(name)?;
        file.lock()
            .with_context(|| format!("locking environment {name}"))?;
        Ok(InstanceLock { _file: file })
    }

    fn write_instance(
        &self,
        name: &InstanceName,
        generation: uuid::Uuid,
        endpoints: &Endpoints,
    ) -> Result<()> {
        let instance = Instance {
            name: name.to_string(),
            version: 1,
            generation,
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

    fn delete_instance_generation(
        &self,
        name: &InstanceName,
        generation: uuid::Uuid,
        host: &dyn StartHost,
    ) -> Result<bool> {
        let path = self.instance_dir(name).join("instance.json");
        if !path.exists() || self.read_instance(name)?.generation != generation {
            return Ok(false);
        }
        host.delete(self, name)?;
        Ok(true)
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
    signal: Arc<AtomicI32>,
    receiver: mpsc::Receiver<i32>,
}

impl Shutdown {
    #[cfg(test)]
    fn from_receiver(receiver: mpsc::Receiver<i32>) -> Self {
        Self {
            signal: Arc::new(AtomicI32::new(0)),
            receiver,
        }
    }

    fn install() -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let signal = Arc::new(AtomicI32::new(0));
        let handler_signal = signal.clone();
        let mut signals = signal_hook::iterator::Signals::new([
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
        ])
        .context("installing shutdown signal handlers")?;
        std::thread::spawn(move || {
            for received in signals.forever() {
                handler_signal.store(received, Ordering::SeqCst);
                let _ = sender.send(received);
            }
        });
        Ok(Self { signal, receiver })
    }

    fn try_interrupted(&self) -> bool {
        if let Ok(signal) = self.receiver.try_recv() {
            self.signal.store(signal, Ordering::SeqCst);
        }
        self.signal.load(Ordering::SeqCst) != 0
    }

    fn received_signal(&self) -> Option<i32> {
        self.try_interrupted();
        match self.signal.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
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
        let signal = self
            .receiver
            .recv()
            .context("waiting for a shutdown signal")?;
        self.signal.store(signal, Ordering::SeqCst);
        Ok(())
    }

    fn wait_timeout(&self, timeout: Duration) -> Result<()> {
        if self.try_interrupted() {
            bail!("interrupted");
        }
        match self.receiver.recv_timeout(timeout) {
            Ok(signal) => {
                self.signal.store(signal, Ordering::SeqCst);
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
    fn instance_exists(&self, runtime: &Runtime, name: &InstanceName) -> Result<bool>;
    fn app_running(&self, container: &str) -> Result<bool>;
    fn runtime_status(
        &self,
        endpoints: &Endpoints,
        request_timeout: Duration,
    ) -> Result<RuntimeStatus>;
    fn delete(&self, runtime: &Runtime, name: &InstanceName) -> Result<()>;
    fn allocate(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        generation: uuid::Uuid,
        deadline: Instant,
        shutdown: &Shutdown,
    ) -> Result<Endpoints>;
    fn wait_ready(
        &self,
        endpoints: &Endpoints,
        app_container: &str,
        deadline: Instant,
        shutdown: &Shutdown,
    ) -> Result<()>;
    fn run_child(
        &self,
        command: &[OsString],
        current_dir: &Path,
        name: &InstanceName,
        endpoints: &Endpoints,
        shutdown: &Shutdown,
    ) -> Result<ChildOutcome>;
    fn diagnostics(&self, name: &InstanceName) -> String;
    fn open_url(&self, url: &str) -> Result<()>;
    fn wait_for_shutdown(&self, shutdown: &Shutdown) -> Result<()>;
}

struct DockerHost;

impl StartHost for DockerHost {
    fn instance_exists(&self, runtime: &Runtime, name: &InstanceName) -> Result<bool> {
        if runtime.instance_dir(name).exists() {
            return Ok(true);
        }
        let resource_label = format!("label={}", label(name));
        let prefix = prefix(name);
        let network_name = format!("name=^{prefix}$");
        let containers = docker_output([
            "container",
            "ls",
            "--all",
            "--filter",
            &resource_label,
            "--format",
            "{{.ID}}",
        ])?;
        let volumes = docker_output([
            "volume",
            "ls",
            "--filter",
            &resource_label,
            "--format",
            "{{.Name}}",
        ])?;
        let networks = docker_output([
            "network",
            "ls",
            "--filter",
            &network_name,
            "--format",
            "{{.Name}}",
        ])?;
        Ok(!containers.is_empty() || !volumes.is_empty() || !networks.is_empty())
    }

    fn app_running(&self, container: &str) -> Result<bool> {
        container_running(container)
    }

    fn runtime_status(
        &self,
        endpoints: &Endpoints,
        request_timeout: Duration,
    ) -> Result<RuntimeStatus> {
        let response = reqwest::blocking::Client::builder()
            .timeout(request_timeout)
            .build()?
            .get(format!("{}/api/v1/status", endpoints.dashboard))
            .send()
            .context("reading runtime status")?;
        let response_status = response.status();
        if !response_status.is_success() {
            let detail = response
                .text()
                .unwrap_or_else(|_| "response body was unreadable".to_owned());
            bail!("runtime status returned {response_status}: {detail}");
        }
        response.json().context("decoding runtime status")
    }

    fn delete(&self, runtime: &Runtime, name: &InstanceName) -> Result<()> {
        runtime.delete_instance_resources(name)
    }

    fn allocate(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        generation: uuid::Uuid,
        deadline: Instant,
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
        wait_for_zakura_tip(&zakura_rpc, &zakura_container, deadline, shutdown)?;
        docker(["start", &format!("{prefix}-lightwalletd")])?;
        shutdown.check()?;
        ensure_app(&prefix, name)?;
        shutdown.check()?;
        docker(["start", &format!("{prefix}-app")])?;
        shutdown.check()?;
        let endpoints = inspect_endpoints(&prefix)?;
        runtime.write_instance(name, generation, &endpoints)?;
        Ok(endpoints)
    }

    fn wait_ready(
        &self,
        endpoints: &Endpoints,
        app_container: &str,
        deadline: Instant,
        shutdown: &Shutdown,
    ) -> Result<()> {
        wait_ready(&endpoints.dashboard, app_container, deadline, shutdown)
    }

    fn run_child(
        &self,
        command: &[OsString],
        current_dir: &Path,
        name: &InstanceName,
        endpoints: &Endpoints,
        shutdown: &Shutdown,
    ) -> Result<ChildOutcome> {
        run_child(
            command,
            current_dir,
            name,
            endpoints,
            shutdown,
            Duration::from_secs(5),
        )
    }

    fn diagnostics(&self, name: &InstanceName) -> String {
        let prefix = prefix(name);
        ["app", "zakura", "lightwalletd"]
            .map(|service| {
                let container = format!("{prefix}-{service}");
                let logs = docker_logs(&container)
                    .unwrap_or_else(|error| format!("could not read logs: {error}"));
                format!("\n--- {service} ---\n{logs}")
            })
            .join("")
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
        let lifecycle = self.lock_instance(name)?;
        if host.instance_exists(self, name)? {
            bail!("environment {name} already exists; stop or reset it before starting again");
        }
        let generation = uuid::Uuid::new_v4();
        let mut cleanup = CleanupOnDrop {
            runtime: self,
            name,
            host,
            active: true,
        };
        println!("Preparing a fresh {name} environment…");
        println!("Starting {name}…");
        let endpoints = host.allocate(
            self,
            name,
            generation,
            Instant::now() + Duration::from_secs(120),
            shutdown,
        )?;
        shutdown.check()?;
        host.wait_ready(
            &endpoints,
            &format!("{}-app", prefix(name)),
            Instant::now() + Duration::from_secs(120),
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
        cleanup.active = false;
        drop(lifecycle);
        if !json {
            println!("\nPress Ctrl+C to stop and delete this development environment.");
        }
        let wait = host.wait_for_shutdown(shutdown);
        let _lifecycle = self.lock_instance_blocking(name)?;
        let deleted = self.delete_instance_generation(name, generation, host);
        match (wait, deleted) {
            (Ok(()), Ok(true)) => {
                println!("\nDeleted {name} and all of its development data.");
                Ok(())
            }
            (Ok(()), Ok(false)) => {
                println!("\n{name} was removed or replaced; no cleanup was needed.");
                Ok(())
            }
            (Ok(()), Err(delete)) => Err(delete),
            (Err(wait), Ok(_)) => Err(wait),
            (Err(wait), Err(delete)) => {
                Err(wait.context(format!("cleanup also failed: {delete:#}")))
            }
        }
    }

    fn run_with(
        &self,
        name: &InstanceName,
        timeout: Duration,
        command: &[OsString],
        current_dir: &Path,
        host: &dyn StartHost,
        shutdown: &Shutdown,
    ) -> Result<u8> {
        let _lock = self.lock_instance(name)?;
        if host.instance_exists(self, name)? {
            bail!("environment {name} already exists");
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("startup timeout is too large")?;
        let mut cleanup = CleanupOnDrop {
            runtime: self,
            name,
            host,
            active: true,
        };
        let endpoints = host.allocate(self, name, uuid::Uuid::new_v4(), deadline, shutdown)?;
        let app_container = format!("{}-app", prefix(name));
        host.wait_ready(&endpoints, &app_container, deadline, shutdown)?;
        wait_until_settled(
            host,
            &endpoints,
            &app_container,
            deadline,
            Duration::from_millis(750),
            shutdown,
            true,
        )?;
        let outcome = host.run_child(command, current_dir, name, &endpoints, shutdown)?;
        if outcome != ChildOutcome::Exited(0) {
            eprintln!("Run diagnostics:{}", host.diagnostics(name));
        }
        let delete_result = host.delete(self, name);
        cleanup.active = false;
        match outcome {
            ChildOutcome::Exited(0) => {
                delete_result?;
                Ok(0)
            }
            ChildOutcome::Exited(code) => {
                if let Err(error) = delete_result {
                    eprintln!("could not delete {name}: {error:#}");
                }
                Ok(code)
            }
            ChildOutcome::Signaled => {
                if let Err(error) = delete_result {
                    eprintln!("could not delete {name}: {error:#}");
                }
                Ok(1)
            }
        }
    }
}

fn display_height(height: Option<u64>) -> String {
    height.map_or_else(|| "unavailable".to_owned(), |height| height.to_string())
}

fn wait_until_settled(
    host: &dyn StartHost,
    endpoints: &Endpoints,
    app_container: &str,
    deadline: Instant,
    poll_interval: Duration,
    shutdown: &Shutdown,
    progress: bool,
) -> Result<RuntimeStatus> {
    let mut last = None;
    let mut last_error = None;
    loop {
        shutdown.check()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let mut detail = last.as_ref().map_or_else(
                || "no status received".to_owned(),
                |status: &RuntimeStatus| {
                    format!(
                        "node {}, lightwalletd {}, wallet {}, blocker {}",
                        display_height(status.node_height),
                        display_height(status.lightwalletd_height),
                        display_height(status.wallet_scanned_height),
                        status.settlement_blocker.as_deref().unwrap_or("unknown")
                    )
                },
            );
            if let Some(error) = last_error {
                detail.push_str(&format!("; last status read failed: {error}"));
            }
            bail!("environment did not settle before the timeout ({detail})");
        }
        if !host.app_running(app_container)? {
            bail!("app exited before the environment settled");
        }
        let status = match host.runtime_status(endpoints, remaining.min(Duration::from_secs(5))) {
            Ok(status) => status,
            Err(error) => {
                let error = format!("{error:#}");
                if progress && last_error.as_deref() != Some(error.as_str()) {
                    eprintln!("Waiting: runtime status unavailable ({error})");
                }
                last_error = Some(error);
                shutdown.wait_timeout(remaining.min(poll_interval))?;
                continue;
            }
        };
        last_error = None;
        if status.settled {
            return Ok(status);
        }
        if progress && last.as_ref() != Some(&status) {
            eprintln!(
                "Waiting: node {}, lightwalletd {}, wallet {} ({})",
                display_height(status.node_height),
                display_height(status.lightwalletd_height),
                display_height(status.wallet_scanned_height),
                status
                    .settlement_blocker
                    .as_deref()
                    .unwrap_or("not settled")
            );
        }
        last = Some(status);
        shutdown.wait_timeout(remaining.min(poll_interval))?;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildOutcome {
    Exited(u8),
    Signaled,
}

fn run_child(
    command: &[OsString],
    current_dir: &Path,
    name: &InstanceName,
    endpoints: &Endpoints,
    shutdown: &Shutdown,
    shutdown_grace: Duration,
) -> Result<ChildOutcome> {
    let (program, arguments) = command
        .split_first()
        .context("run requires a child command")?;
    let mut process = Command::new(program);
    process
        .args(arguments)
        .current_dir(current_dir)
        .env("TSZ_INSTANCE", name.to_string())
        .env("TSZ_NETWORK", "regtest")
        .env("TSZ_API_URL", &endpoints.dashboard)
        .env("TSZ_ZAKURA_RPC_URL", &endpoints.rpc)
        .env("TSZ_LIGHTWALLETD_URL", &endpoints.lightwalletd)
        .env("TSZ_LIGHTWALLETD_TLS", "false")
        .env("TSZ_P2P_ADDR", &endpoints.p2p);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
    let mut child = process
        .spawn()
        .with_context(|| format!("starting child command {}", program.to_string_lossy()))?;
    loop {
        if let Some(status) = child.try_wait().context("waiting for child command")? {
            return Ok(
                match status.code().and_then(|code| u8::try_from(code).ok()) {
                    Some(code) => ChildOutcome::Exited(code),
                    None => ChildOutcome::Signaled,
                },
            );
        }
        if let Some(signal) = shutdown.received_signal() {
            terminate_child_group(&mut child, signal, shutdown_grace)?;
            return Ok(ChildOutcome::Signaled);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn terminate_child_group(
    child: &mut std::process::Child,
    signal: i32,
    grace: Duration,
) -> Result<()> {
    use nix::{
        errno::Errno,
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };

    let pgid = Pid::from_raw(i32::try_from(child.id()).context("child process ID is too large")?);
    let signal = Signal::try_from(signal).context("unsupported shutdown signal")?;
    if let Err(error) = killpg(pgid, signal)
        && error != Errno::ESRCH
    {
        return Err(error).context("forwarding shutdown signal to child process group");
    }

    let deadline = Instant::now() + grace;
    let mut reaped = false;
    loop {
        if !reaped
            && child
                .try_wait()
                .context("waiting for child command")?
                .is_some()
        {
            reaped = true;
        }
        match killpg(pgid, None) {
            Err(Errno::ESRCH) => break,
            Err(error) => return Err(error).context("checking child process group"),
            Ok(()) if Instant::now() >= deadline => {
                killpg(pgid, Signal::SIGKILL)
                    .or_else(|error| (error == Errno::ESRCH).then_some(()).ok_or(error))
                    .context("force-terminating child process group")?;
                break;
            }
            Ok(()) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    if !reaped {
        child.wait().context("reaping child command")?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn terminate_child_group(
    child: &mut std::process::Child,
    _signal: i32,
    _grace: Duration,
) -> Result<()> {
    child.kill().context("terminating child command")?;
    child.wait().context("reaping child command")?;
    Ok(())
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
    deadline: Instant,
    shutdown: &Shutdown,
) -> Result<()> {
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
    bail!("dashboard did not become healthy before the startup timeout")
}
fn wait_for_zakura_tip(
    base: &str,
    container: &str,
    deadline: Instant,
    shutdown: &Shutdown,
) -> Result<()> {
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
    bail!("Zakura RPC tip did not become available before the startup timeout")
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
        instance_exists: bool,
        app_running: bool,
        statuses: Mutex<std::collections::VecDeque<std::result::Result<RuntimeStatus, String>>>,
        child_outcome: ChildOutcome,
        delete_result: Result<(), String>,
        wait_ready_result: Result<(), String>,
        wait_ready_delay: Duration,
        open_url_result: Result<(), String>,
        interrupt_before_ready: bool,
    }

    impl RecordingHost {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    events: events.clone(),
                    instance_exists: false,
                    app_running: true,
                    statuses: Mutex::new(std::collections::VecDeque::from([Ok(runtime_status(
                        true, None,
                    ))])),
                    child_outcome: ChildOutcome::Exited(0),
                    delete_result: Ok(()),
                    wait_ready_result: Ok(()),
                    wait_ready_delay: Duration::ZERO,
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
        fn instance_exists(&self, _runtime: &Runtime, name: &InstanceName) -> Result<bool> {
            self.push(&format!("exists:{name}"));
            Ok(self.instance_exists)
        }

        fn app_running(&self, container: &str) -> Result<bool> {
            self.push(&format!("app_running:{container}"));
            Ok(self.app_running)
        }

        fn runtime_status(
            &self,
            _endpoints: &Endpoints,
            _request_timeout: Duration,
        ) -> Result<RuntimeStatus> {
            self.push("runtime_status");
            self.statuses
                .lock()
                .unwrap()
                .pop_front()
                .context("no recorded runtime status")?
                .map_err(anyhow::Error::msg)
        }

        fn delete(&self, _runtime: &Runtime, name: &InstanceName) -> Result<()> {
            self.push(&format!("delete:{name}"));
            self.delete_result
                .as_ref()
                .map(|_| ())
                .map_err(|error| anyhow!(error.clone()))
        }

        fn allocate(
            &self,
            runtime: &Runtime,
            name: &InstanceName,
            generation: uuid::Uuid,
            _deadline: Instant,
            shutdown: &Shutdown,
        ) -> Result<Endpoints> {
            self.push(&format!("allocate:{name}"));
            shutdown.check()?;
            let endpoints = Endpoints {
                dashboard: "http://127.0.0.1:1".into(),
                rpc: "http://127.0.0.1:2".into(),
                lightwalletd: "http://127.0.0.1:3".into(),
                p2p: "127.0.0.1:4".into(),
            };
            fs::create_dir_all(runtime.instance_dir(name))?;
            runtime.write_instance(name, generation, &endpoints)?;
            Ok(endpoints)
        }

        fn wait_ready(
            &self,
            _endpoints: &Endpoints,
            _app_container: &str,
            _deadline: Instant,
            shutdown: &Shutdown,
        ) -> Result<()> {
            self.push("wait_ready");
            std::thread::sleep(self.wait_ready_delay);
            if self.interrupt_before_ready {
                bail!("interrupted");
            }
            shutdown.check()?;
            self.wait_ready_result
                .as_ref()
                .map(|_| ())
                .map_err(|e| anyhow!("{e}"))
        }

        fn run_child(
            &self,
            _command: &[OsString],
            _current_dir: &Path,
            name: &InstanceName,
            _endpoints: &Endpoints,
            _shutdown: &Shutdown,
        ) -> Result<ChildOutcome> {
            self.push(&format!("child:{name}"));
            Ok(self.child_outcome)
        }

        fn diagnostics(&self, name: &InstanceName) -> String {
            self.push(&format!("diagnostics:{name}"));
            "recorded diagnostics".into()
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
            root: std::env::temp_dir().join(format!(
                "tsz-start-cleanup-tests-{}",
                uuid::Uuid::new_v4().simple()
            )),
        }
    }

    fn name(value: &str) -> InstanceName {
        value.parse().unwrap()
    }

    fn runtime_status(settled: bool, blocker: Option<&str>) -> RuntimeStatus {
        RuntimeStatus {
            node_height: Some(105),
            lightwalletd_height: Some(105),
            wallet_scanned_height: Some(if settled { 105 } else { 104 }),
            wallet_sync_state: if settled { "ready" } else { "syncing" }.into(),
            settled,
            settlement_blocker: blocker.map(str::to_owned),
        }
    }

    #[test]
    fn wait_until_settled_polls_until_all_components_agree() {
        let (mut host, events) = RecordingHost::new();
        host.statuses = Mutex::new(std::collections::VecDeque::from([
            Ok(runtime_status(false, Some("wallet_behind"))),
            Ok(runtime_status(true, None)),
        ]));
        let (_sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);

        let status = wait_until_settled(
            &host,
            &Endpoints::default(),
            "tsz-alpha-app",
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(1),
            &shutdown,
            false,
        )
        .unwrap();

        assert!(status.settled);
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.as_str() == "runtime_status")
                .count(),
            2
        );
    }

    #[test]
    fn wait_until_settled_retries_a_status_read_failure() {
        let (mut host, events) = RecordingHost::new();
        host.statuses = Mutex::new(std::collections::VecDeque::from([
            Err("temporary status failure".into()),
            Ok(runtime_status(true, None)),
        ]));
        let (_sender, receiver) = std::sync::mpsc::channel();

        let status = wait_until_settled(
            &host,
            &Endpoints::default(),
            "tsz-alpha-app",
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(1),
            &Shutdown::from_receiver(receiver),
            false,
        )
        .unwrap();

        assert!(status.settled);
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.as_str() == "runtime_status")
                .count(),
            2
        );
    }

    #[test]
    fn wait_until_settled_reports_last_progress_on_timeout() {
        let (mut host, _) = RecordingHost::new();
        host.statuses = Mutex::new(std::collections::VecDeque::from([Ok(runtime_status(
            false,
            Some("wallet_behind"),
        ))]));
        let (_sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);

        let error = wait_until_settled(
            &host,
            &Endpoints::default(),
            "tsz-alpha-app",
            Instant::now() + Duration::from_millis(5),
            Duration::from_millis(10),
            &shutdown,
            false,
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("node 105"));
        assert!(message.contains("lightwalletd 105"));
        assert!(message.contains("wallet 104"));
        assert!(message.contains("wallet_behind"));
    }

    #[test]
    fn wait_until_settled_stops_on_exit_and_interrupt() {
        let (mut exited, _) = RecordingHost::new();
        exited.app_running = false;
        let (_sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        let error = wait_until_settled(
            &exited,
            &Endpoints::default(),
            "tsz-alpha-app",
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(1),
            &shutdown,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("app exited"));

        let (mut transport, _) = RecordingHost::new();
        transport.statuses = Mutex::new(Default::default());
        let (_sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        let error = wait_until_settled(
            &transport,
            &Endpoints::default(),
            "tsz-alpha-app",
            Instant::now() + Duration::from_millis(5),
            Duration::from_millis(1),
            &shutdown,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("did not settle"));
        assert!(error.to_string().contains("no recorded runtime status"));

        let (host, _) = RecordingHost::new();
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(signal_hook::consts::SIGINT).unwrap();
        let shutdown = Shutdown::from_receiver(receiver);
        let error = wait_until_settled(
            &host,
            &Endpoints::default(),
            "tsz-alpha-app",
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(1),
            &shutdown,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("interrupted"));
    }

    #[test]
    fn child_inherits_project_context_and_receives_runtime_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("child-environment");
        let command = vec![
            std::ffi::OsString::from("sh"),
            std::ffi::OsString::from("-c"),
            std::ffi::OsString::from("{ pwd; env; } > \"$1\""),
            std::ffi::OsString::from("sh"),
            output.as_os_str().to_owned(),
        ];
        let endpoints = Endpoints {
            dashboard: "http://127.0.0.1:8080".into(),
            rpc: "http://127.0.0.1:18232".into(),
            lightwalletd: "http://127.0.0.1:9067".into(),
            p2p: "127.0.0.1:18233".into(),
        };
        let (_sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);

        let outcome = run_child(
            &command,
            dir.path(),
            &name("alpha"),
            &endpoints,
            &shutdown,
            Duration::from_millis(20),
        )
        .unwrap();

        assert_eq!(outcome, ChildOutcome::Exited(0));
        let output = std::fs::read_to_string(output).unwrap();
        assert!(
            output
                .lines()
                .next()
                .unwrap()
                .ends_with(dir.path().to_str().unwrap())
        );
        for expected in [
            "TSZ_INSTANCE=alpha",
            "TSZ_NETWORK=regtest",
            "TSZ_API_URL=http://127.0.0.1:8080",
            "TSZ_ZAKURA_RPC_URL=http://127.0.0.1:18232",
            "TSZ_LIGHTWALLETD_URL=http://127.0.0.1:9067",
            "TSZ_LIGHTWALLETD_TLS=false",
            "TSZ_P2P_ADDR=127.0.0.1:18233",
        ] {
            assert!(
                output.lines().any(|line| line == expected),
                "missing {expected}"
            );
        }
        assert!(output.lines().any(|line| line.starts_with("PATH=")));
        assert!(!output.lines().any(|line| line.starts_with("TSZ_SEED=")));
    }

    #[test]
    fn child_exit_code_is_preserved() {
        let command = [
            std::ffi::OsString::from("sh"),
            std::ffi::OsString::from("-c"),
            std::ffi::OsString::from("exit 7"),
        ];
        let (_sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);

        let outcome = run_child(
            &command,
            std::path::Path::new("."),
            &name("alpha"),
            &Endpoints::default(),
            &shutdown,
            Duration::from_millis(20),
        )
        .unwrap();

        assert_eq!(outcome, ChildOutcome::Exited(7));
    }

    #[test]
    fn run_rejects_a_collision_without_mutating_it() {
        let (mut host, events) = RecordingHost::new();
        host.instance_exists = true;
        let (_sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);

        let error = runtime_for_tests()
            .run_with(
                &name("alpha"),
                Duration::from_secs(1),
                &[OsString::from("true")],
                Path::new("."),
                &host,
                &shutdown,
            )
            .unwrap_err();

        assert!(error.to_string().contains("already exists"));
        assert_eq!(&*events.lock().unwrap(), &["exists:alpha"]);
    }

    #[test]
    fn run_orders_start_settlement_child_diagnostics_and_cleanup() {
        let (mut host, events) = RecordingHost::new();
        host.child_outcome = ChildOutcome::Exited(7);
        let (_sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);

        let code = runtime_for_tests()
            .run_with(
                &name("alpha"),
                Duration::from_secs(1),
                &[OsString::from("false")],
                Path::new("."),
                &host,
                &shutdown,
            )
            .unwrap();

        assert_eq!(code, 7);
        assert_eq!(
            &*events.lock().unwrap(),
            &[
                "exists:alpha",
                "allocate:alpha",
                "wait_ready",
                "app_running:tsz-alpha-app",
                "runtime_status",
                "child:alpha",
                "diagnostics:alpha",
                "delete:alpha",
            ]
        );
    }

    #[test]
    fn run_cleanup_failure_obeys_child_exit_precedence() {
        let (mut successful_host, _) = RecordingHost::new();
        successful_host.delete_result = Err("cleanup failed".into());
        let (_sender, receiver) = std::sync::mpsc::channel();
        let error = runtime_for_tests()
            .run_with(
                &name("success"),
                Duration::from_secs(1),
                &[OsString::from("true")],
                Path::new("."),
                &successful_host,
                &Shutdown::from_receiver(receiver),
            )
            .unwrap_err();
        assert!(error.to_string().contains("cleanup failed"));

        let (mut failed_host, events) = RecordingHost::new();
        failed_host.child_outcome = ChildOutcome::Exited(9);
        failed_host.delete_result = Err("cleanup failed".into());
        let (_sender, receiver) = std::sync::mpsc::channel();
        let code = runtime_for_tests()
            .run_with(
                &name("failure"),
                Duration::from_secs(1),
                &[OsString::from("false")],
                Path::new("."),
                &failed_host,
                &Shutdown::from_receiver(receiver),
            )
            .unwrap();
        assert_eq!(code, 9);
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.as_str() == "delete:failure")
                .count(),
            1
        );
    }

    #[test]
    fn run_uses_one_startup_and_settlement_deadline() {
        let (mut host, events) = RecordingHost::new();
        host.wait_ready_delay = Duration::from_millis(10);
        let (_sender, receiver) = std::sync::mpsc::channel();
        let error = runtime_for_tests()
            .run_with(
                &name("alpha"),
                Duration::from_millis(1),
                &[OsString::from("true")],
                Path::new("."),
                &host,
                &Shutdown::from_receiver(receiver),
            )
            .unwrap_err();

        assert!(error.to_string().contains("did not settle"));
        let events = events.lock().unwrap();
        assert!(!events.iter().any(|event| event == "runtime_status"));
        assert!(!events.iter().any(|event| event.starts_with("child:")));
        assert_eq!(
            events
                .iter()
                .filter(|event| event.as_str() == "delete:alpha")
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn child_interruption_terminates_background_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("descendant.pid");
        let command = vec![
            std::ffi::OsString::from("sh"),
            std::ffi::OsString::from("-c"),
            std::ffi::OsString::from("trap '' INT; sleep 30 & echo $! > \"$1\"; wait"),
            std::ffi::OsString::from("sh"),
            pid_file.as_os_str().to_owned(),
        ];
        let (sender, receiver) = std::sync::mpsc::channel();
        let signal_file = pid_file.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !signal_file.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            sender.send(signal_hook::consts::SIGINT).unwrap();
        });
        let shutdown = Shutdown::from_receiver(receiver);

        let outcome = run_child(
            &command,
            dir.path(),
            &name("alpha"),
            &Endpoints::default(),
            &shutdown,
            Duration::from_millis(50),
        )
        .unwrap();

        assert_eq!(outcome, ChildOutcome::Signaled);
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let pid = pid.trim();
        std::thread::sleep(Duration::from_millis(20));
        let alive = std::process::Command::new("kill")
            .args(["-0", pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if alive {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", pid])
                .status();
        }
        assert!(!alive, "background descendant {pid} survived interruption");
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
            sender.send(signal_hook::consts::SIGINT).unwrap();
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
    fn start_releases_the_lifecycle_lock_while_running() {
        let runtime = Arc::new(runtime_for_tests());
        let (host, events) = RecordingHost::new();
        let host = Arc::new(host);
        let (sender, receiver) = std::sync::mpsc::channel();
        let task_runtime = Arc::clone(&runtime);
        let task_host = Arc::clone(&host);
        let task = std::thread::spawn(move || {
            task_runtime.start_with(
                &name("alpha"),
                true,
                false,
                task_host.as_ref(),
                &Shutdown::from_receiver(receiver),
            )
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "wait_for_shutdown")
        {
            assert!(
                Instant::now() < deadline,
                "start did not reach its idle state"
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        runtime.lock_instance(&name("alpha")).unwrap();
        sender.send(signal_hook::consts::SIGINT).unwrap();
        task.join().unwrap().unwrap();
    }

    #[test]
    fn start_refuses_an_existing_instance_without_deleting_it() {
        let (mut host, events) = RecordingHost::new();
        host.instance_exists = true;
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(signal_hook::consts::SIGINT).unwrap();
        let shutdown = Shutdown::from_receiver(receiver);

        let error = runtime_for_tests()
            .start_with(&name("alpha"), true, false, &host, &shutdown)
            .unwrap_err();

        assert!(error.to_string().contains("already exists"));
        let events = events.lock().unwrap();
        assert!(!events.iter().any(|event| event == "delete:alpha"));
        assert!(!events.iter().any(|event| event == "allocate:alpha"));
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
        sender.send(signal_hook::consts::SIGINT).unwrap();
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
    fn instance_lock_excludes_concurrent_lifecycle_changes() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: root.path().to_path_buf(),
        };
        let first = runtime.lock_instance(&name("alpha")).unwrap();

        let error = runtime.lock_instance(&name("alpha")).unwrap_err();
        assert!(error.to_string().contains("already in use"));

        drop(first);
        runtime.lock_instance(&name("alpha")).unwrap();
    }

    #[test]
    fn destructive_commands_respect_the_instance_lock() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: root.path().to_path_buf(),
        };
        let stop_name = name(&format!("stop-{}", uuid::Uuid::new_v4().simple()));
        let stop_lock = runtime.lock_instance(&stop_name).unwrap();
        assert!(
            runtime
                .stop(&stop_name)
                .unwrap_err()
                .to_string()
                .contains("already in use")
        );
        drop(stop_lock);

        let reset_name = name(&format!("reset-{}", uuid::Uuid::new_v4().simple()));
        let reset_lock = runtime.lock_instance(&reset_name).unwrap();
        assert!(
            runtime
                .reset(&reset_name, true)
                .unwrap_err()
                .to_string()
                .contains("already in use")
        );
        drop(reset_lock);
    }

    #[test]
    fn stale_start_cannot_delete_a_replacement_instance() {
        let runtime = runtime_for_tests();
        let name = name("alpha");
        let stale = uuid::Uuid::new_v4();
        let current = uuid::Uuid::new_v4();
        fs::create_dir_all(runtime.instance_dir(&name)).unwrap();
        fs::write(
            runtime.instance_dir(&name).join("instance.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "alpha",
                "version": 1,
                "generation": current,
                "endpoints": Endpoints::default(),
            }))
            .unwrap(),
        )
        .unwrap();
        let (host, events) = RecordingHost::new();

        assert!(
            !runtime
                .delete_instance_generation(&name, stale, &host)
                .unwrap()
        );
        assert!(events.lock().unwrap().is_empty());
        assert!(
            runtime
                .delete_instance_generation(&name, current, &host)
                .unwrap()
        );
        assert_eq!(events.lock().unwrap().as_slice(), ["delete:alpha"]);
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
        sender.send(signal_hook::consts::SIGINT).unwrap();
        assert!(shutdown.try_interrupted());
        assert!(shutdown.try_interrupted());
        shutdown.wait().unwrap();
    }

    #[test]
    fn shutdown_check_bails_when_latched() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        shutdown.check().unwrap();
        sender.send(signal_hook::consts::SIGINT).unwrap();
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
            sender.send(signal_hook::consts::SIGINT).unwrap();
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
        sender.send(signal_hook::consts::SIGINT).unwrap();
        shutdown.wait().unwrap();
    }

    #[test]
    fn wait_ready_aborts_when_shutdown_is_signaled() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        sender.send(signal_hook::consts::SIGINT).unwrap();
        let err = wait_ready(
            "http://127.0.0.1:1",
            "missing-app",
            Instant::now() + Duration::from_secs(5),
            &shutdown,
        )
        .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
    }

    #[test]
    fn wait_for_zakura_tip_aborts_when_shutdown_is_signaled() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        sender.send(signal_hook::consts::SIGINT).unwrap();
        let err = wait_for_zakura_tip(
            "http://127.0.0.1:1",
            "missing-zakura",
            Instant::now() + Duration::from_secs(5),
            &shutdown,
        )
        .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
    }
}
