use std::{
    fs,
    io::Read,
    net::TcpListener as StdTcpListener,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, bail, ensure};
use async_trait::async_trait;
use reqwest::{Client, redirect::Policy};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::{Child, Command};
use uuid::Uuid;

use super::{
    GenerateFaultProxy, checked_u16, is_startup_pending, request_json, rpc_with_timeout,
    safe_exit_code,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const INIT_TIMEOUT: Duration = Duration::from_secs(120);
const NODE_READY_TIMEOUT: Duration = Duration::from_secs(120);
const FUNDED_STARTUP_TIMEOUT: Duration = Duration::from_secs(300);
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(120);
const ASSERT_RUNNING_TIMEOUT: Duration = Duration::from_secs(90);
const API_READ_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(10);
const PROCESS_KILL_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const FALLBACK_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub(crate) struct CommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    /// Retained only for exact, local Docker not-found classification.
    /// It is never surfaced in diagnostics or error text.
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// The only command seam in the fixture. It is per-instance and test-only.
#[async_trait]
pub(crate) trait CommandExecutor: Send + Sync {
    async fn execute(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
        capture_stdout: bool,
    ) -> Result<CommandOutput>;
}

#[derive(Default)]
struct TokioCommandExecutor;

#[async_trait]
impl CommandExecutor for TokioCommandExecutor {
    async fn execute(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
        capture_stdout: bool,
    ) -> Result<CommandOutput> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if capture_stdout {
            command.stdout(Stdio::piped());
        } else {
            command.stdout(Stdio::null());
        }
        let output = tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| anyhow::anyhow!("fixture command timed out"))?
            .map_err(|_| anyhow::anyhow!("fixture command could not be started"))?;
        Ok(CommandOutput {
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[derive(Clone, Debug)]
struct OwnedNames {
    prefix: String,
    network: String,
    chain_volume: String,
    lightwalletd_volume: String,
    node_container: String,
    lightwalletd_container: String,
}

impl OwnedNames {
    fn new() -> Self {
        let prefix = format!("tsz-recovery-{}", Uuid::new_v4());
        Self {
            network: prefix.clone(),
            chain_volume: format!("{prefix}-chain"),
            lightwalletd_volume: format!("{prefix}-lightwalletd"),
            node_container: format!("{prefix}-zakura"),
            lightwalletd_container: format!("{prefix}-lightwalletd"),
            prefix,
        }
    }

    fn as_json(&self) -> Value {
        json!({
            "chain_volume": self.chain_volume,
            "lightwalletd_container": self.lightwalletd_container,
            "lightwalletd_volume": self.lightwalletd_volume,
            "network": self.network,
            "node_container": self.node_container,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DockerResource {
    Network(String),
    Volume(String),
    Container(String),
}

impl DockerResource {
    fn kind(&self) -> &'static str {
        match self {
            Self::Network(_) => "network",
            Self::Volume(_) => "volume",
            Self::Container(_) => "container",
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Network(name) | Self::Volume(name) | Self::Container(name) => name,
        }
    }

    fn label(&self) -> String {
        format!("Docker {} {}", self.kind(), self.name())
    }

    fn inspect_args(&self) -> Vec<String> {
        vec![self.kind().into(), "inspect".into(), self.name().into()]
    }

    fn remove_args(&self) -> Vec<String> {
        match self {
            Self::Network(name) => vec!["network".into(), "rm".into(), name.clone()],
            Self::Volume(name) => {
                vec!["volume".into(), "rm".into(), "--force".into(), name.clone()]
            }
            Self::Container(name) => {
                vec![
                    "container".into(),
                    "rm".into(),
                    "--force".into(),
                    name.clone(),
                ]
            }
        }
    }

    fn exact_not_found_stderr(&self) -> Vec<u8> {
        match self {
            Self::Network(name) => format!("Error: No such network: {name}\n").into_bytes(),
            Self::Volume(name) => {
                format!("Error response from daemon: get {name}: no such volume\n").into_bytes()
            }
            Self::Container(name) => format!("Error: No such container: {name}\n").into_bytes(),
        }
    }

    fn rootless_not_found_stderr(&self) -> Vec<u8> {
        match self {
            Self::Network(name) => {
                format!("Error response from daemon: network {name} not found\n").into_bytes()
            }
            Self::Volume(name) => {
                format!("Error response from daemon: get {name}: no such volume\n").into_bytes()
            }
            Self::Container(name) => {
                format!("Error response from daemon: No such container: {name}\n").into_bytes()
            }
        }
    }
}

/// Returns false only for Docker's exact, resource-specific not-found message.
/// Any other failed inspection (including CLI, daemon, permission or timeout
/// failure) is deliberately ambiguous and must retain cleanup ownership.
fn docker_resource_exists_from_inspect(
    resource: &DockerResource,
    output: &CommandOutput,
) -> Result<bool> {
    if output.succeeded() {
        return Ok(true);
    }
    if (output.stdout.is_empty() && output.stderr == resource.exact_not_found_stderr())
        || (output.stdout == b"[]\n" && output.stderr == resource.rootless_not_found_stderr())
    {
        return Ok(false);
    }
    bail!("could not verify whether an owned Docker resource exists")
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CleanupResource {
    PrivateDirectory,
    Docker(DockerResource),
    Proxy,
    Server,
}

impl CleanupResource {
    fn label(&self) -> String {
        match self {
            Self::PrivateDirectory => "private fixture directory".to_owned(),
            Self::Docker(resource) => resource.label(),
            Self::Proxy => "fixture RPC proxy".to_owned(),
            Self::Server => "fixture server process".to_owned(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct DockerInspect {
    #[serde(rename = "State")]
    state: DockerState,
    #[serde(rename = "NetworkSettings")]
    network_settings: DockerNetworkSettings,
}

#[derive(Debug, Deserialize)]
struct DockerState {
    #[serde(rename = "Running")]
    running: bool,
}

#[derive(Debug, Deserialize)]
struct DockerNetworkSettings {
    #[serde(rename = "Ports")]
    ports: std::collections::BTreeMap<String, Option<Vec<DockerPortBinding>>>,
}

#[derive(Debug, Deserialize)]
struct DockerPortBinding {
    #[serde(rename = "HostIp")]
    host_ip: String,
    #[serde(rename = "HostPort")]
    host_port: String,
}

#[derive(Debug, Deserialize)]
struct HealthResponse {
    instance: String,
    wallet_sync: WalletSyncResponse,
}

#[derive(Debug, Deserialize)]
struct WalletSyncResponse {
    state: String,
}

#[derive(Debug, Deserialize)]
struct AccountBalance {
    id: u8,
    orchard_zatoshi: u64,
}

/// Owns an isolated Zakura node, lightwalletd, proxy and `tsz-server` process.
pub struct RegtestStack {
    server: PathBuf,
    temporary_directory: Option<TempDir>,
    data_dir: PathBuf,
    config_dir: PathBuf,
    names: OwnedNames,
    executor: Arc<dyn CommandExecutor>,
    fallback_cleanup_enabled: bool,
    client: Client,
    cleanup: Vec<CleanupResource>,
    proxy: Option<GenerateFaultProxy>,
    server_child: Option<Child>,
    node_url: String,
    api_url: String,
    start_attempted: bool,
    shutdown_complete: bool,
    cleanup_errors: Vec<String>,
    server_exit_code: Option<i32>,
}

impl RegtestStack {
    pub fn new(server: PathBuf) -> Result<Self> {
        Self::with_executor_and_fallback(server, Arc::new(TokioCommandExecutor), true)
    }

    pub(crate) fn with_executor(
        server: PathBuf,
        executor: Arc<dyn CommandExecutor>,
    ) -> Result<Self> {
        Self::with_executor_and_fallback(server, executor, false)
    }

    fn with_executor_and_fallback(
        server: PathBuf,
        executor: Arc<dyn CommandExecutor>,
        fallback_cleanup_enabled: bool,
    ) -> Result<Self> {
        let temp_root = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let temporary_directory = TempDir::new_in(temp_root)
            .map_err(|_| anyhow::anyhow!("creating private fixture directory"))?;
        let data_dir = temporary_directory.path().join("data");
        let config_dir = temporary_directory.path().join("config");
        make_private_directory(&data_dir)?;
        make_private_directory(&config_dir)?;
        let client = Client::builder()
            .redirect(Policy::none())
            .build()
            .map_err(|_| anyhow::anyhow!("creating fixture HTTP client"))?;

        Ok(Self {
            server,
            temporary_directory: Some(temporary_directory),
            data_dir,
            config_dir,
            names: OwnedNames::new(),
            executor,
            fallback_cleanup_enabled,
            client,
            cleanup: vec![CleanupResource::PrivateDirectory],
            proxy: None,
            server_child: None,
            node_url: String::new(),
            api_url: String::new(),
            start_attempted: false,
            shutdown_complete: false,
            cleanup_errors: Vec::new(),
            server_exit_code: None,
        })
    }

    pub async fn start(&mut self) -> Result<()> {
        ensure!(
            !self.start_attempted,
            "fixture startup may only be attempted once per owned stack"
        );
        self.start_attempted = true;
        self.require_server_path()?;
        self.docker_checked(
            vec![
                "version".into(),
                "--format".into(),
                "{{.Server.Version}}".into(),
            ],
            COMMAND_TIMEOUT,
            false,
            "checking Docker",
        )
        .await?;
        self.server_checked(
            vec![
                "init".into(),
                "--data-dir".into(),
                self.data_dir.display().to_string(),
                "--config-dir".into(),
                self.config_dir.display().to_string(),
            ],
            INIT_TIMEOUT,
            "initializing the isolated server data",
        )
        .await?;

        let network = DockerResource::Network(self.names.network.clone());
        let chain_volume = DockerResource::Volume(self.names.chain_volume.clone());
        let lightwalletd_volume = DockerResource::Volume(self.names.lightwalletd_volume.clone());
        self.claim_docker_resource(network.clone()).await?;
        self.docker_checked(
            vec!["network".into(), "create".into(), network.name().into()],
            COMMAND_TIMEOUT,
            false,
            "creating the fixture network",
        )
        .await?;
        self.claim_docker_resource(chain_volume.clone()).await?;
        self.docker_checked(
            vec!["volume".into(), "create".into(), chain_volume.name().into()],
            COMMAND_TIMEOUT,
            false,
            "creating the fixture chain volume",
        )
        .await?;
        self.claim_docker_resource(lightwalletd_volume.clone())
            .await?;
        self.docker_checked(
            vec![
                "volume".into(),
                "create".into(),
                lightwalletd_volume.name().into(),
            ],
            COMMAND_TIMEOUT,
            false,
            "creating the fixture lightwalletd volume",
        )
        .await?;

        let node = DockerResource::Container(self.names.node_container.clone());
        self.claim_docker_resource(node).await?;
        self.docker_checked(
            vec![
                "run".into(),
                "--detach".into(),
                "--name".into(),
                self.names.node_container.clone(),
                "--network".into(),
                self.names.network.clone(),
                "--network-alias".into(),
                "zakura".into(),
                "--volume".into(),
                format!("{}:/data", self.names.chain_volume),
                "--volume".into(),
                format!("{}:/config:ro", self.config_dir.display()),
                "--env".into(),
                "CONFIG_FILE_PATH=/config/zakurad.toml".into(),
                // The private host-mounted config is mode 0700. Keep the
                // image entrypoint from dropping to its default UID so the
                // isolated node can read that config while preserving its
                // private host permissions.
                "--env".into(),
                "UID=0".into(),
                "--env".into(),
                "GID=0".into(),
                "--publish".into(),
                "127.0.0.1::18232".into(),
                "zakuracore/zakura:1.4.0".into(),
                "zakurad".into(),
                "start".into(),
            ],
            COMMAND_TIMEOUT,
            false,
            "starting the fixture Zakura node",
        )
        .await?;
        let node_inspect = self.inspect_container(&self.names.node_container).await?;
        let node_port = published_loopback_port(&node_inspect, "18232/tcp")?;
        self.node_url = format!("http://127.0.0.1:{node_port}");
        self.wait_for_node_ready().await?;

        let lightwalletd = DockerResource::Container(self.names.lightwalletd_container.clone());
        self.claim_docker_resource(lightwalletd).await?;
        self.docker_checked(
            vec![
                "run".into(),
                "--detach".into(),
                "--name".into(),
                self.names.lightwalletd_container.clone(),
                "--network".into(),
                self.names.network.clone(),
                "--user".into(),
                "0:0".into(),
                "--volume".into(),
                format!("{}:/var/lib/lightwalletd", self.names.lightwalletd_volume),
                "--publish".into(),
                "127.0.0.1::9067".into(),
                "tsz-recovery-lightwalletd:local".into(),
                "--no-tls-very-insecure".into(),
                "--grpc-bind-addr".into(),
                "0.0.0.0:9067".into(),
                "--rpchost".into(),
                "zakura".into(),
                "--rpcport".into(),
                "18232".into(),
                "--rpcuser".into(),
                "unused".into(),
                "--rpcpassword".into(),
                "unused".into(),
                "--data-dir".into(),
                "/var/lib/lightwalletd".into(),
                "--log-file".into(),
                "/dev/stdout".into(),
            ],
            COMMAND_TIMEOUT,
            false,
            "starting fixture lightwalletd",
        )
        .await?;
        let lightwalletd_inspect = self
            .inspect_container(&self.names.lightwalletd_container)
            .await?;
        let lightwalletd_port = published_loopback_port(&lightwalletd_inspect, "9067/tcp")?;

        self.cleanup.push(CleanupResource::Proxy);
        self.proxy = Some(GenerateFaultProxy::start(self.node_url.clone()).await?);
        let api_port = select_loopback_port()?;
        self.api_url = format!("http://127.0.0.1:{api_port}");
        self.cleanup.push(CleanupResource::Server);
        self.spawn_server(api_port, lightwalletd_port).await?;
        self.wait_for_funded_server().await
    }

    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    pub fn node_url(&self) -> &str {
        &self.node_url
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn proxy(&self) -> &GenerateFaultProxy {
        self.proxy
            .as_ref()
            .expect("fixture proxy is available only after successful startup")
    }

    /// Checks owned recovery services using the fixture's bounded liveness budget.
    pub async fn assert_running(&mut self) -> Result<()> {
        self.assert_running_before(Instant::now() + ASSERT_RUNNING_TIMEOUT)
            .await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        if self.shutdown_complete {
            return Ok(());
        }

        let cleanup = self.cleanup.iter().cloned().rev().collect::<Vec<_>>();
        let mut errors = Vec::new();
        for resource in cleanup {
            let result = match resource.clone() {
                CleanupResource::PrivateDirectory => self.remove_private_directory(),
                CleanupResource::Docker(resource) => self.remove_docker_resource(&resource).await,
                CleanupResource::Proxy => match self.proxy.as_mut() {
                    Some(proxy) => proxy.shutdown().await,
                    None => Ok(()),
                },
                CleanupResource::Server => self.stop_server().await,
            };
            if result.is_err() {
                errors.push(resource.label());
            } else {
                self.cleanup.retain(|candidate| candidate != &resource);
            }
        }
        self.cleanup_errors = errors;
        self.shutdown_complete = self.cleanup.is_empty();
        if self.shutdown_complete {
            Ok(())
        } else {
            bail!("fixture cleanup failed for owned resources")
        }
    }

    pub(crate) fn recovery_deadline() -> Instant {
        Instant::now() + RECOVERY_TIMEOUT
    }

    /// Performs one recovery status/activity read within the shared deadline.
    ///
    /// Callers retry only classified transport errors; schema, API, and
    /// assertion failures remain ordinary immediate errors from this method.
    pub(crate) async fn recovery_read<T: DeserializeOwned>(
        &mut self,
        deadline: Instant,
        path: &str,
    ) -> Result<T> {
        self.assert_running_before(deadline).await?;
        request_json(
            &self.client,
            &self.api_url,
            path,
            None,
            remaining_read_timeout(deadline)?,
        )
        .await
    }

    pub(crate) fn cleanup_errors(&self) -> &[String] {
        &self.cleanup_errors
    }

    fn remove_private_directory(&mut self) -> Result<()> {
        let Some(directory) = self.temporary_directory.as_ref() else {
            return Ok(());
        };
        let path = directory.path().to_path_buf();
        fs::remove_dir_all(&path)
            .map_err(|_| anyhow::anyhow!("removing private fixture directory"))?;
        ensure!(
            !path
                .try_exists()
                .map_err(|_| anyhow::anyhow!("checking private fixture directory cleanup"))?,
            "private fixture directory still exists after cleanup"
        );
        self.temporary_directory.take();
        Ok(())
    }

    fn require_server_path(&self) -> Result<()> {
        ensure!(
            self.server.is_file(),
            "Cargo-provided tsz-server path is not a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&self.server)
                .map_err(|_| anyhow::anyhow!("reading Cargo-provided tsz-server permissions"))?
                .permissions()
                .mode();
            ensure!(
                mode & 0o111 != 0,
                "Cargo-provided tsz-server path is not executable"
            );
        }
        Ok(())
    }

    async fn claim_docker_resource(&mut self, resource: DockerResource) -> Result<()> {
        if self.docker_resource_exists(&resource).await? {
            bail!("generated fixture Docker resource name is already in use");
        }
        // This comes before create/run: Docker can create a named resource and
        // still return a non-zero result to its caller.
        self.cleanup.push(CleanupResource::Docker(resource));
        Ok(())
    }

    async fn docker_resource_exists(&self, resource: &DockerResource) -> Result<bool> {
        let output = self
            .executor
            .execute("docker", &resource.inspect_args(), COMMAND_TIMEOUT, true)
            .await?;
        docker_resource_exists_from_inspect(resource, &output)
    }

    async fn docker_checked(
        &self,
        args: Vec<String>,
        timeout: Duration,
        capture_stdout: bool,
        label: &'static str,
    ) -> Result<CommandOutput> {
        let output = self
            .executor
            .execute("docker", &args, timeout, capture_stdout)
            .await?;
        if !output.succeeded() {
            bail!(
                "{label} exited with status {}",
                safe_exit_code(output.exit_code)
            );
        }
        Ok(output)
    }

    async fn server_checked(
        &self,
        args: Vec<String>,
        timeout: Duration,
        label: &'static str,
    ) -> Result<()> {
        let program = self.server.to_string_lossy().into_owned();
        let output = self
            .executor
            .execute(&program, &args, timeout, false)
            .await?;
        if !output.succeeded() {
            bail!(
                "{label} exited with status {}",
                safe_exit_code(output.exit_code)
            );
        }
        Ok(())
    }

    async fn inspect_container(&self, name: &str) -> Result<DockerInspect> {
        self.inspect_container_with_timeout(name, COMMAND_TIMEOUT)
            .await
    }

    async fn inspect_container_with_timeout(
        &self,
        name: &str,
        timeout: Duration,
    ) -> Result<DockerInspect> {
        let output = self
            .docker_checked(
                vec!["inspect".into(), name.to_owned()],
                timeout,
                true,
                "inspecting owned fixture container",
            )
            .await?;
        let mut inspect: Vec<DockerInspect> = serde_json::from_slice(&output.stdout)
            .map_err(|_| anyhow::anyhow!("Docker inspection returned invalid JSON"))?;
        ensure!(
            inspect.len() == 1,
            "Docker inspection did not return one owned container"
        );
        Ok(inspect.remove(0))
    }

    async fn assert_container_running_with_timeout(
        &self,
        name: &str,
        timeout: Duration,
    ) -> Result<()> {
        let inspect = self.inspect_container_with_timeout(name, timeout).await?;
        ensure!(
            inspect.state.running,
            "owned fixture container is not running"
        );
        Ok(())
    }

    async fn assert_running_before(&mut self, deadline: Instant) -> Result<()> {
        self.assert_server_running()?;
        self.assert_container_running_with_timeout(
            &self.names.node_container,
            remaining_timeout(deadline, COMMAND_TIMEOUT)?,
        )
        .await?;
        self.assert_container_running_with_timeout(
            &self.names.lightwalletd_container,
            remaining_timeout(deadline, COMMAND_TIMEOUT)?,
        )
        .await
    }

    async fn wait_for_node_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + NODE_READY_TIMEOUT;
        loop {
            self.assert_container_running_with_timeout(
                &self.names.node_container,
                remaining_timeout(deadline, COMMAND_TIMEOUT)?,
            )
            .await?;
            match rpc_with_timeout::<Value>(
                &self.client,
                &self.node_url,
                "getblockchaininfo",
                json!([]),
                remaining_timeout(deadline, COMMAND_TIMEOUT)?,
            )
            .await
            {
                Ok(_) => return Ok(()),
                Err(error) if is_startup_pending(&error) && Instant::now() < deadline => {
                    sleep_to_next_poll(deadline).await;
                }
                Err(error) => return Err(error.context("waiting for fixture node readiness")),
            }
            if Instant::now() >= deadline {
                bail!("fixture node readiness exceeded its 120-second deadline");
            }
        }
    }

    async fn wait_for_funded_server(&mut self) -> Result<()> {
        let deadline = Instant::now() + FUNDED_STARTUP_TIMEOUT;
        loop {
            self.assert_running_before(deadline).await?;
            let health = request_json::<HealthResponse>(
                &self.client,
                &self.api_url,
                "/api/v1/health",
                None,
                remaining_read_timeout(deadline)?,
            )
            .await;
            let accounts = request_json::<Vec<AccountBalance>>(
                &self.client,
                &self.api_url,
                "/api/v1/accounts",
                None,
                remaining_read_timeout(deadline)?,
            )
            .await;
            match (health, accounts) {
                (Ok(health), Ok(accounts)) => {
                    ensure!(
                        health.instance == self.names.prefix,
                        "fixture API ownership did not match its generated instance"
                    );
                    let funded = accounts
                        .iter()
                        .any(|account| account.id == 1 && account.orchard_zatoshi == 500_000_000);
                    if health.wallet_sync.state == "ready" && funded {
                        return Ok(());
                    }
                }
                (Err(error), _) | (_, Err(error))
                    if is_startup_pending(&error) && Instant::now() < deadline => {}
                (Err(error), _) | (_, Err(error)) => {
                    return Err(error.context("waiting for funded fixture startup"));
                }
            }
            if Instant::now() >= deadline {
                bail!("funded fixture startup exceeded its 300-second deadline");
            }
            sleep_to_next_poll(deadline).await;
        }
    }

    async fn spawn_server(&mut self, api_port: u16, lightwalletd_port: u16) -> Result<()> {
        let proxy_url = self.proxy().url().to_owned();
        let mut command = Command::new(&self.server);
        command
            .arg("serve")
            .arg("--data-dir")
            .arg(&self.data_dir)
            .env("TSZ_ZAKURA_RPC", proxy_url)
            .env(
                "TSZ_LIGHTWALLETD",
                format!("http://127.0.0.1:{lightwalletd_port}"),
            )
            .env("TSZ_INSTANCE", &self.names.prefix)
            .env("TSZ_LISTEN", format!("127.0.0.1:{api_port}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let child = command
            .spawn()
            .map_err(|_| anyhow::anyhow!("starting the owned fixture server"))?;
        self.server_child = Some(child);
        Ok(())
    }

    fn assert_server_running(&mut self) -> Result<()> {
        if self.server_exit_code.is_some() {
            return Err(self.server_exit_error());
        }
        let child = self.server_child.as_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "owned fixture server was not started at selected endpoint {}; refusing an unowned listener",
                self.api_url
            )
        })?;
        if let Some(status) = child
            .try_wait()
            .map_err(|_| anyhow::anyhow!("checking owned fixture server state"))?
        {
            self.server_exit_code = status.code();
            return Err(self.server_exit_error());
        }
        Ok(())
    }

    fn server_exit_error(&self) -> anyhow::Error {
        anyhow::anyhow!(
            "owned fixture server exited before startup at selected endpoint {}; refusing an unowned listener",
            self.api_url
        )
    }

    async fn stop_server(&mut self) -> Result<()> {
        let exit_code = {
            let Some(child) = self.server_child.as_mut() else {
                return Ok(());
            };
            if let Some(status) = child
                .try_wait()
                .map_err(|_| anyhow::anyhow!("checking owned fixture server during cleanup"))?
            {
                status.code()
            } else {
                child
                    .start_kill()
                    .map_err(|_| anyhow::anyhow!("stopping owned fixture server"))?;
                match tokio::time::timeout(PROCESS_STOP_TIMEOUT, child.wait()).await {
                    Ok(Ok(status)) => status.code(),
                    Ok(Err(_)) => bail!("reaping owned fixture server failed"),
                    Err(_) => {
                        let _ = child.start_kill();
                        match tokio::time::timeout(PROCESS_KILL_TIMEOUT, child.wait()).await {
                            Ok(Ok(status)) => status.code(),
                            Ok(Err(_)) | Err(_) => {
                                bail!("owned fixture server could not be reaped")
                            }
                        }
                    }
                }
            }
        };
        self.server_exit_code = exit_code;
        self.server_child.take();
        Ok(())
    }

    async fn remove_docker_resource(&self, resource: &DockerResource) -> Result<()> {
        let output = self
            .executor
            .execute("docker", &resource.remove_args(), COMMAND_TIMEOUT, false)
            .await?;
        let exists = self.docker_resource_exists(resource).await?;
        if !exists {
            return Ok(());
        }
        if !output.succeeded() {
            bail!(
                "removing owned Docker resource exited with status {}",
                safe_exit_code(output.exit_code)
            );
        }
        bail!("owned Docker resource still exists after cleanup")
    }

    fn server_exit_value(&self) -> Value {
        match (self.server_exit_code, self.server_child.is_some()) {
            (Some(code), _) => json!(code),
            (None, true) => json!("running"),
            (None, false)
                if self
                    .cleanup
                    .iter()
                    .any(|resource| matches!(resource, CleanupResource::Server)) =>
            {
                json!("unavailable")
            }
            (None, false) => json!("not-reached"),
        }
    }
}

impl Drop for RegtestStack {
    fn drop(&mut self) {
        if self.shutdown_complete || !self.fallback_cleanup_enabled {
            return;
        }
        let cleanup = self.cleanup.iter().cloned().rev().collect::<Vec<_>>();
        let mut complete = true;
        for resource in cleanup {
            match resource {
                CleanupResource::Server => {
                    if let Some(child) = self.server_child.as_mut() {
                        let _ = child.start_kill();
                        complete = false;
                    }
                }
                CleanupResource::Proxy => {
                    // The proxy retains its listener JoinHandle in a bounded
                    // fallback reaper; do not destroy it before handoff.
                    if self
                        .proxy
                        .as_mut()
                        .is_some_and(|proxy| !proxy.fallback_shutdown())
                    {
                        complete = false;
                    }
                }
                CleanupResource::Docker(resource) => {
                    if !synchronous_remove_resource(&resource) {
                        complete = false;
                    }
                }
                CleanupResource::PrivateDirectory => {
                    if synchronous_remove_private_directory(self.temporary_directory.as_ref()) {
                        self.temporary_directory.take();
                    } else {
                        complete = false;
                    }
                }
            }
        }
        if !complete {
            eprintln!("ACTIVITY_RECOVERY_FALLBACK_CLEANUP_INCOMPLETE");
        }
    }
}

/// The fixed allowlist for a live test's sanitized failure record.
#[derive(Clone, Copy, Debug)]
pub enum RecoveryPhase {
    Setup,
    Broadcast,
    AutoMine,
    DirectMine,
    Recovery,
    Cleanup,
}

impl RecoveryPhase {
    fn label(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Broadcast => "broadcast",
            Self::AutoMine => "auto-mine",
            Self::DirectMine => "direct-mine",
            Self::Recovery => "recovery",
            Self::Cleanup => "cleanup",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum HeightCheckpoint {
    BeforeAutoMine,
    AfterAutoMine,
    Inclusion,
    Tip,
    Scanned,
}

#[derive(Clone, Copy, Debug)]
pub enum FailureRoute {
    Setup,
    Signal,
    Cleanup,
    Error,
}

impl FailureRoute {
    fn label(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Signal => "signal",
            Self::Cleanup => "cleanup",
            Self::Error => "error",
        }
    }
}

/// Records only allowlisted values; it never accepts an error/body/log string.
pub struct RecoveryFailureReporter {
    phase: RecoveryPhase,
    before_auto_mine: Option<u64>,
    after_auto_mine: Option<u64>,
    inclusion: Option<u64>,
    tip: Option<u64>,
    scanned: Option<u64>,
    activity_id: SafeDiagnosticText,
    activity_status: SafeDiagnosticText,
    txid: SafeDiagnosticText,
}

#[derive(Clone, Debug)]
enum SafeDiagnosticText {
    NotReached,
    Value(String),
    Unavailable,
}

impl SafeDiagnosticText {
    fn json(&self) -> Value {
        match self {
            Self::NotReached => json!("not-reached"),
            Self::Value(value) => json!(value),
            Self::Unavailable => json!("unavailable"),
        }
    }
}

impl Default for RecoveryFailureReporter {
    fn default() -> Self {
        Self {
            phase: RecoveryPhase::Setup,
            before_auto_mine: None,
            after_auto_mine: None,
            inclusion: None,
            tip: None,
            scanned: None,
            activity_id: SafeDiagnosticText::NotReached,
            activity_status: SafeDiagnosticText::NotReached,
            txid: SafeDiagnosticText::NotReached,
        }
    }
}

impl RecoveryFailureReporter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn phase(&mut self, phase: RecoveryPhase) {
        self.phase = phase;
    }

    pub fn record_height(&mut self, checkpoint: HeightCheckpoint, height: u64) {
        match checkpoint {
            HeightCheckpoint::BeforeAutoMine => self.before_auto_mine = Some(height),
            HeightCheckpoint::AfterAutoMine => self.after_auto_mine = Some(height),
            HeightCheckpoint::Inclusion => self.inclusion = Some(height),
            HeightCheckpoint::Tip => self.tip = Some(height),
            HeightCheckpoint::Scanned => self.scanned = Some(height),
        }
    }

    pub fn record_activity(&mut self, activity_id: &str, status: &str, txid: &str) {
        self.activity_id = if Uuid::parse_str(activity_id).is_ok() {
            SafeDiagnosticText::Value(activity_id.to_owned())
        } else {
            SafeDiagnosticText::Unavailable
        };
        self.activity_status = match status {
            "broadcast" | "confirmed" => SafeDiagnosticText::Value(status.to_owned()),
            _ => SafeDiagnosticText::Unavailable,
        };
        self.txid = if is_txid(txid) {
            SafeDiagnosticText::Value(txid.to_owned())
        } else {
            SafeDiagnosticText::Unavailable
        };
    }

    pub fn summary(&self, stack: Option<&RegtestStack>, route: FailureRoute) -> Value {
        let heights = json!({
            "after_auto_mine": self.after_auto_mine.map_or_else(|| json!("not-reached"), |value| json!(value)),
            "before_auto_mine": self.before_auto_mine.map_or_else(|| json!("not-reached"), |value| json!(value)),
            "inclusion": self.inclusion.map_or_else(|| json!("not-reached"), |value| json!(value)),
            "scanned": self.scanned.map_or_else(|| json!("not-reached"), |value| json!(value)),
            "tip": self.tip.map_or_else(|| json!("not-reached"), |value| json!(value)),
        });
        let (owned_resources, server_exit, cleanup, proxy_counts) = match stack {
            Some(stack) => {
                let cleanup = if !stack.cleanup_errors().is_empty() {
                    "failed"
                } else if stack.shutdown_complete {
                    "complete"
                } else {
                    "not-reached"
                };
                let proxy_counts = stack.proxy.as_ref().map_or_else(
                    || json!({"forwarded": "not-reached", "rejected": "not-reached"}),
                    |proxy| {
                        let counts = proxy.counts();
                        json!({"forwarded": counts.forwarded, "rejected": counts.rejected})
                    },
                );
                (
                    stack.names.as_json(),
                    stack.server_exit_value(),
                    json!(cleanup),
                    proxy_counts,
                )
            }
            None => (
                json!({
                    "chain_volume": "not-reached",
                    "lightwalletd_container": "not-reached",
                    "lightwalletd_volume": "not-reached",
                    "network": "not-reached",
                    "node_container": "not-reached",
                }),
                json!("not-reached"),
                json!("not-reached"),
                json!({"forwarded": "not-reached", "rejected": "not-reached"}),
            ),
        };
        json!({
            "activity": {"id": self.activity_id.json(), "status": self.activity_status.json()},
            "cleanup": cleanup,
            "exit_codes": {"server": server_exit},
            "failure_route": route.label(),
            "heights": heights,
            "owned_resources": owned_resources,
            "phase": self.phase.label(),
            "proxy_counts": proxy_counts,
            "txid": self.txid.json(),
        })
    }

    pub fn emit(&self, stack: Option<&RegtestStack>, route: FailureRoute) {
        eprintln!(
            "ACTIVITY_RECOVERY_FAILURE_SUMMARY {}",
            self.summary(stack, route)
        );
    }
}

/// Linux-only Tokio signal registration for the ignored live target.
pub struct TerminationSignals {
    #[cfg(target_os = "linux")]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(target_os = "linux")]
    terminate: tokio::signal::unix::Signal,
}

impl TerminationSignals {
    pub fn install() -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let interrupt = signal(SignalKind::interrupt())
                .map_err(|_| anyhow::anyhow!("installing fixture SIGINT handler"))?;
            let terminate = signal(SignalKind::terminate())
                .map_err(|_| anyhow::anyhow!("installing fixture SIGTERM handler"))?;
            Ok(Self {
                interrupt,
                terminate,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            bail!("fixture interruption support requires Linux Tokio signals")
        }
    }

    pub async fn cancelled(&mut self) {
        #[cfg(target_os = "linux")]
        {
            tokio::select! {
                _ = self.interrupt.recv() => {}
                _ = self.terminate.recv() => {}
            }
        }
    }
}

/// Test-only cancellation seam: it receives a local one-shot signal and takes
/// the same explicit cleanup path as the live cancellation owner.
#[cfg(test)]
async fn shutdown_after_test_cancellation(
    stack: &mut RegtestStack,
    cancellation: &mut tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    let _ = cancellation.await;
    stack.shutdown().await
}

fn published_loopback_port(inspect: &DockerInspect, container_port: &str) -> Result<u16> {
    let bindings = inspect
        .network_settings
        .ports
        .get(container_port)
        .and_then(Option::as_ref)
        .ok_or_else(|| {
            anyhow::anyhow!("Docker inspection did not contain the required port binding")
        })?;
    let binding = bindings
        .iter()
        .find(|binding| binding.host_ip == "127.0.0.1")
        .ok_or_else(|| anyhow::anyhow!("Docker inspection did not publish the port on loopback"))?;
    checked_u16(&binding.host_port)
}

fn make_private_directory(path: &Path) -> Result<()> {
    fs::create_dir(path).map_err(|_| anyhow::anyhow!("creating private fixture subdirectory"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| anyhow::anyhow!("protecting private fixture subdirectory"))?;
    }
    Ok(())
}

fn select_loopback_port() -> Result<u16> {
    let listener = StdTcpListener::bind("127.0.0.1:0")
        .map_err(|_| anyhow::anyhow!("reserving a fixture loopback port"))?;
    let port = listener
        .local_addr()
        .map_err(|_| anyhow::anyhow!("reading reserved fixture loopback port"))?
        .port();
    drop(listener);
    Ok(port)
}

fn remaining_read_timeout(deadline: Instant) -> Result<Duration> {
    remaining_timeout(deadline, API_READ_TIMEOUT)
}

fn remaining_timeout(deadline: Instant, maximum: Duration) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "fixture phase exceeded its deadline");
    Ok(remaining.min(maximum))
}

async fn sleep_to_next_poll(deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    tokio::time::sleep(remaining.min(POLL_INTERVAL)).await;
}

fn is_txid(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn synchronous_remove_private_directory(directory: Option<&TempDir>) -> bool {
    let Some(directory) = directory else {
        return true;
    };
    let path = directory.path();
    fs::remove_dir_all(path).is_ok() && path.try_exists().map(|exists| !exists).unwrap_or(false)
}

fn synchronous_remove_resource(resource: &DockerResource) -> bool {
    let removal = synchronous_docker_output(&resource.remove_args());
    let inspection = synchronous_docker_output(&resource.inspect_args());
    let Ok(exists) = inspection
        .as_ref()
        .ok_or(())
        .and_then(|output| docker_resource_exists_from_inspect(resource, output).map_err(|_| ()))
    else {
        return false;
    };

    // A failed rm is tolerable only when the exact subsequent inspect proves
    // the resource is already absent (for example, a concurrent daemon race).
    // Inspecting the status keeps a nonzero removal from being silently treated
    // as success when absence cannot be established.
    match removal {
        Some(output) if output.succeeded() => !exists,
        Some(_) => !exists,
        None => false,
    }
}

fn synchronous_docker_output(args: &[String]) -> Option<CommandOutput> {
    synchronous_command_output("docker", args)
}

fn synchronous_command_output(program: &str, args: &[String]) -> Option<CommandOutput> {
    synchronous_command_output_with_timeout(program, args, FALLBACK_COMMAND_TIMEOUT)
}

fn synchronous_command_output_with_timeout(
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Option<CommandOutput> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let stderr = child.stderr.take()?;
    let (sender, receiver) = std::sync::mpsc::channel();
    let stdout_reader = std::thread::spawn({
        let sender = sender.clone();
        move || {
            let mut stdout = stdout;
            let mut bytes = Vec::new();
            let _ = sender.send((true, stdout.read_to_end(&mut bytes).ok().map(|_| bytes)));
        }
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut stderr = stderr;
        let mut bytes = Vec::new();
        let _ = sender.send((false, stderr.read_to_end(&mut bytes).ok().map(|_| bytes)));
    });
    let deadline = Instant::now() + timeout;
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut stdout = None;
    let mut stderr = None;
    for _ in 0..2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match receiver.recv_timeout(remaining).ok()? {
            (true, Some(bytes)) => stdout = Some(bytes),
            (false, Some(bytes)) => stderr = Some(bytes),
            (_, None) => return None,
        }
    }
    stdout_reader.join().ok()?;
    stderr_reader.join().ok()?;
    Some(CommandOutput {
        exit_code,
        stdout: stdout?,
        stderr: stderr?,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::{Value, json};

    #[cfg(target_os = "linux")]
    use super::synchronous_command_output_with_timeout;
    use super::{
        CleanupResource, CommandExecutor, CommandOutput, DockerInspect, DockerResource,
        FailureRoute, RecoveryFailureReporter, RegtestStack, docker_resource_exists_from_inspect,
        published_loopback_port, remaining_timeout, shutdown_after_test_cancellation,
        synchronous_command_output,
    };

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Invocation {
        program: String,
        args: Vec<String>,
        capture_stdout: bool,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum InspectionFailure {
        ExecutorTimeout,
        NonzeroStderr(Vec<u8>),
    }

    #[derive(Default)]
    struct RecordingExecutor {
        calls: Mutex<Vec<Invocation>>,
        fail_node_run: AtomicBool,
        failing_cleanup_name: Mutex<Option<String>>,
        failed_removal_once_name: Mutex<Option<String>>,
        inspection_failures: Mutex<HashMap<String, InspectionFailure>>,
        removal_attempts: Mutex<HashMap<String, usize>>,
        exited_container: AtomicBool,
    }

    impl RecordingExecutor {
        fn set_failing_cleanup_name(&self, name: String) {
            *self.failing_cleanup_name.lock().unwrap() = Some(name);
        }

        fn set_failed_removal_once_name(&self, name: String) {
            *self.failed_removal_once_name.lock().unwrap() = Some(name);
        }

        fn set_inspection_failure(&self, name: String, failure: InspectionFailure) {
            self.inspection_failures
                .lock()
                .unwrap()
                .insert(name, failure);
        }

        fn calls(&self) -> Vec<Invocation> {
            self.calls.lock().unwrap().clone()
        }

        fn resource_inspect_status(&self, name: &str) -> i32 {
            let attempts = self
                .removal_attempts
                .lock()
                .unwrap()
                .get(name)
                .copied()
                .unwrap_or_default();
            let fails_once =
                self.failing_cleanup_name.lock().unwrap().as_deref() == Some(name) && attempts == 1;
            let failed_removal_keeps_resource =
                self.failed_removal_once_name.lock().unwrap().as_deref() == Some(name)
                    && attempts == 1;
            if fails_once || failed_removal_keeps_resource {
                0
            } else {
                1
            }
        }

        fn exited_inspection() -> Vec<u8> {
            serde_json::to_vec(&json!([{
                "State": {"Running": false},
                "NetworkSettings": {"Ports": {}},
            }]))
            .expect("fixed Docker inspection JSON serializes")
        }
    }

    #[async_trait]
    impl CommandExecutor for RecordingExecutor {
        async fn execute(
            &self,
            program: &str,
            args: &[String],
            _timeout: Duration,
            capture_stdout: bool,
        ) -> Result<CommandOutput> {
            self.calls.lock().unwrap().push(Invocation {
                program: program.to_owned(),
                args: args.to_vec(),
                capture_stdout,
            });

            if program != "docker" {
                return Ok(CommandOutput {
                    exit_code: Some(0),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            if args.first().is_some_and(|argument| argument == "run")
                && args
                    .iter()
                    .any(|argument| argument == "zakuracore/zakura:1.4.0")
                && self.fail_node_run.load(Ordering::SeqCst)
            {
                return Ok(CommandOutput {
                    exit_code: Some(17),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            if args.first().is_some_and(|argument| argument == "inspect")
                && self.exited_container.load(Ordering::SeqCst)
            {
                return Ok(CommandOutput {
                    exit_code: Some(0),
                    stdout: Self::exited_inspection(),
                    stderr: Vec::new(),
                });
            }
            if args.get(1).is_some_and(|argument| argument == "rm") {
                let exit_code = if let Some(name) = args.last() {
                    let mut attempts = self.removal_attempts.lock().unwrap();
                    let attempt = attempts.entry(name.clone()).or_default();
                    *attempt += 1;
                    if self.failed_removal_once_name.lock().unwrap().as_deref() == Some(name)
                        && *attempt == 1
                    {
                        Some(23)
                    } else {
                        Some(0)
                    }
                } else {
                    Some(0)
                };
                return Ok(CommandOutput {
                    exit_code,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            if args.get(1).is_some_and(|argument| argument == "inspect") {
                let name = args
                    .last()
                    .expect("fixture Docker inspection always has an exact owned name");
                if let Some(failure) = self.inspection_failures.lock().unwrap().get(name).cloned() {
                    return match failure {
                        InspectionFailure::ExecutorTimeout => {
                            Err(anyhow::anyhow!("fixture command timed out"))
                        }
                        InspectionFailure::NonzeroStderr(stderr) => Ok(CommandOutput {
                            exit_code: Some(70),
                            stdout: Vec::new(),
                            stderr,
                        }),
                    };
                }
                let status = self.resource_inspect_status(name);
                let stderr = if status == 1 {
                    match args.first().map(String::as_str) {
                        Some("network") => format!("Error: No such network: {name}\n").into_bytes(),
                        Some("volume") => {
                            format!("Error response from daemon: get {name}: no such volume\n")
                                .into_bytes()
                        }
                        Some("container") => {
                            format!("Error: No such container: {name}\n").into_bytes()
                        }
                        _ => Vec::new(),
                    }
                } else {
                    Vec::new()
                };
                return Ok(CommandOutput {
                    exit_code: Some(status),
                    stdout: Vec::new(),
                    stderr,
                });
            }
            Ok(CommandOutput {
                exit_code: Some(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    fn test_server_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_tsz-server"))
    }

    #[test]
    fn regtest_stack_is_sync_for_the_spawned_live_scenario() {
        fn assert_sync<T: Sync>() {}
        assert_sync::<RegtestStack>();
    }

    #[test]
    fn published_loopback_port_requires_one_valid_loopback_binding() -> Result<()> {
        let valid: DockerInspect = serde_json::from_value(json!({
            "State": {"Running": true},
            "NetworkSettings": {
                "Ports": {
                    "18232/tcp": [
                        {"HostIp": "0.0.0.0", "HostPort": "19132"},
                        {"HostIp": "127.0.0.1", "HostPort": "19133"},
                    ],
                },
            },
        }))?;
        assert_eq!(published_loopback_port(&valid, "18232/tcp")?, 19133);

        let missing: DockerInspect = serde_json::from_value(json!({
            "State": {"Running": true},
            "NetworkSettings": {"Ports": {}},
        }))?;
        assert!(published_loopback_port(&missing, "18232/tcp").is_err());

        let non_loopback: DockerInspect = serde_json::from_value(json!({
            "State": {"Running": true},
            "NetworkSettings": {
                "Ports": {"18232/tcp": [{"HostIp": "0.0.0.0", "HostPort": "19132"}]},
            },
        }))?;
        assert!(published_loopback_port(&non_loopback, "18232/tcp").is_err());

        let malformed: DockerInspect = serde_json::from_value(json!({
            "State": {"Running": true},
            "NetworkSettings": {
                "Ports": {"18232/tcp": [{"HostIp": "127.0.0.1", "HostPort": "not-a-port"}]},
            },
        }))?;
        assert!(published_loopback_port(&malformed, "18232/tcp").is_err());
        Ok(())
    }

    #[test]
    fn rootless_docker_not_found_output_is_absent() -> Result<()> {
        let cases = [
            (
                DockerResource::Network("test-network".to_owned()),
                b"Error response from daemon: network test-network not found\n".as_slice(),
            ),
            (
                DockerResource::Volume("test-volume".to_owned()),
                b"Error response from daemon: get test-volume: no such volume\n".as_slice(),
            ),
            (
                DockerResource::Container("test-container".to_owned()),
                b"Error response from daemon: No such container: test-container\n".as_slice(),
            ),
        ];

        for (resource, stderr) in cases {
            let output = CommandOutput {
                exit_code: Some(1),
                stdout: b"[]\n".to_vec(),
                stderr: stderr.to_vec(),
            };
            assert!(
                !docker_resource_exists_from_inspect(&resource, &output)?,
                "rootless Docker must classify a missing {} as absent",
                resource.kind(),
            );
        }
        Ok(())
    }

    #[test]
    fn synchronous_command_output_retains_child_stdout() {
        let output = synchronous_command_output("printf", &["[]\n".to_owned()])
            .expect("local printf command must complete");

        assert_eq!(output.stdout, b"[]\n");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn synchronous_command_output_does_not_wait_for_descendant_held_pipes() {
        let timeout = Duration::from_millis(150);
        let started = Instant::now();
        let output = synchronous_command_output_with_timeout(
            "sh",
            &["-c".to_owned(), "sleep 1 & exit 0".to_owned()],
            timeout,
        );

        assert!(output.is_none(), "the descendant keeps both pipes open");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "stream collection must honor the injected deadline"
        );
    }

    #[tokio::test]
    async fn failed_partial_startup_cleans_exact_names_in_reverse_order_and_retries() -> Result<()>
    {
        let executor = Arc::new(RecordingExecutor::default());
        executor.fail_node_run.store(true, Ordering::SeqCst);
        let fixture_executor: Arc<dyn CommandExecutor> = executor.clone();
        let mut fixture = RegtestStack::with_executor(test_server_path(), fixture_executor)?;
        let expected_names = vec![
            fixture.names.node_container.clone(),
            fixture.names.lightwalletd_volume.clone(),
            fixture.names.chain_volume.clone(),
            fixture.names.network.clone(),
        ];
        executor.set_failing_cleanup_name(fixture.names.lightwalletd_volume.clone());

        assert!(fixture.start().await.is_err());
        let node_run = executor
            .calls()
            .into_iter()
            .find(|call| {
                call.program == "docker"
                    && call.args.first().is_some_and(|argument| argument == "run")
                    && call.args.windows(2).any(|arguments| {
                        arguments[0] == "--name" && arguments[1] == expected_names[0]
                    })
            })
            .expect("fixture must invoke the exact owned node container");
        assert!(
            node_run
                .args
                .windows(2)
                .any(|arguments| arguments[0] == "--env" && arguments[1] == "UID=0")
        );
        assert!(
            node_run
                .args
                .windows(2)
                .any(|arguments| arguments[0] == "--env" && arguments[1] == "GID=0")
        );
        assert!(fixture.shutdown().await.is_err());
        let first_removals = executor
            .calls()
            .into_iter()
            .filter(|call| {
                call.program == "docker"
                    && call.args.get(1).is_some_and(|argument| argument == "rm")
            })
            .filter_map(|call| call.args.last().cloned())
            .collect::<Vec<_>>();
        assert_eq!(first_removals, expected_names);
        assert!(
            first_removals
                .iter()
                .all(|name| name.starts_with("tsz-recovery-"))
        );
        assert_eq!(fixture.cleanup.len(), 1);

        fixture.shutdown().await?;
        assert!(fixture.cleanup.is_empty());
        assert!(fixture.cleanup_errors().is_empty());
        let removals = executor
            .calls()
            .into_iter()
            .filter(|call| {
                call.program == "docker"
                    && call.args.get(1).is_some_and(|argument| argument == "rm")
            })
            .filter_map(|call| call.args.last().cloned())
            .collect::<Vec<_>>();
        assert_eq!(
            removals,
            [
                expected_names[0].clone(),
                expected_names[1].clone(),
                expected_names[2].clone(),
                expected_names[3].clone(),
                expected_names[1].clone(),
            ]
        );
        Ok(())
    }

    async fn assert_ambiguous_inspection_retains_cleanup_ownership(
        resource_kind: &str,
        failure: InspectionFailure,
    ) -> Result<()> {
        let executor = Arc::new(RecordingExecutor::default());
        let fixture_executor: Arc<dyn CommandExecutor> = executor.clone();
        let mut fixture = RegtestStack::with_executor(test_server_path(), fixture_executor)?;
        let resource = match resource_kind {
            "network" => DockerResource::Network(fixture.names.network.clone()),
            "volume" => DockerResource::Volume(fixture.names.chain_volume.clone()),
            "container" => DockerResource::Container(fixture.names.node_container.clone()),
            _ => panic!("test must use a supported Docker resource kind"),
        };
        fixture
            .cleanup
            .push(CleanupResource::Docker(resource.clone()));
        executor.set_inspection_failure(resource.name().to_owned(), failure);

        assert!(
            fixture.docker_resource_exists(&resource).await.is_err(),
            "an ambiguous Docker inspection must not mean resource absence"
        );
        assert_eq!(
            fixture.cleanup,
            vec![
                CleanupResource::PrivateDirectory,
                CleanupResource::Docker(resource),
            ]
        );
        assert!(!fixture.shutdown_complete);
        assert!(executor.calls().iter().all(|call| {
            call.program != "docker" || !call.args.get(1).is_some_and(|argument| argument == "rm")
        }));
        Ok(())
    }

    #[tokio::test]
    async fn exact_not_found_inspection_is_absent_for_each_owned_resource_kind() -> Result<()> {
        let executor = Arc::new(RecordingExecutor::default());
        let fixture_executor: Arc<dyn CommandExecutor> = executor.clone();
        let fixture = RegtestStack::with_executor(test_server_path(), fixture_executor)?;
        let resources = [
            DockerResource::Network(fixture.names.network.clone()),
            DockerResource::Volume(fixture.names.chain_volume.clone()),
            DockerResource::Container(fixture.names.node_container.clone()),
        ];

        for resource in &resources {
            assert!(
                !fixture.docker_resource_exists(resource).await?,
                "exact Docker not-found output must prove {} absence",
                resource.kind()
            );
        }
        assert_eq!(fixture.cleanup, vec![CleanupResource::PrivateDirectory]);
        assert!(executor.calls().iter().all(|call| {
            call.program != "docker" || !call.args.get(1).is_some_and(|argument| argument == "rm")
        }));
        assert!(
            executor
                .calls()
                .iter()
                .filter(|call| {
                    call.program == "docker"
                        && call
                            .args
                            .get(1)
                            .is_some_and(|argument| argument == "inspect")
                })
                .all(|call| call.capture_stdout),
            "owned Docker inspections must retain the rootless not-found stdout form"
        );
        Ok(())
    }

    #[tokio::test]
    async fn daemon_unavailable_inspection_retains_cleanup_ownership() -> Result<()> {
        assert_ambiguous_inspection_retains_cleanup_ownership(
            "network",
            InspectionFailure::NonzeroStderr(b"fixture Docker daemon unavailable\n".to_vec()),
        )
        .await
    }

    #[tokio::test]
    async fn permission_denied_inspection_retains_cleanup_ownership() -> Result<()> {
        assert_ambiguous_inspection_retains_cleanup_ownership(
            "volume",
            InspectionFailure::NonzeroStderr(
                b"permission denied while trying to connect to the Docker daemon socket\n".to_vec(),
            ),
        )
        .await
    }

    #[tokio::test]
    async fn executor_timeout_inspection_retains_cleanup_ownership() -> Result<()> {
        assert_ambiguous_inspection_retains_cleanup_ownership(
            "container",
            InspectionFailure::ExecutorTimeout,
        )
        .await
    }

    #[tokio::test]
    async fn other_nonzero_inspection_failure_retains_cleanup_ownership() -> Result<()> {
        assert_ambiguous_inspection_retains_cleanup_ownership(
            "network",
            InspectionFailure::NonzeroStderr(b"Docker CLI configuration failed\n".to_vec()),
        )
        .await
    }

    #[tokio::test]
    async fn failed_docker_removal_is_retained_while_later_cleanup_continues() -> Result<()> {
        let executor = Arc::new(RecordingExecutor::default());
        let fixture_executor: Arc<dyn CommandExecutor> = executor.clone();
        let mut fixture = RegtestStack::with_executor(test_server_path(), fixture_executor)?;
        let network = DockerResource::Network(fixture.names.network.clone());
        let volume = DockerResource::Volume(fixture.names.chain_volume.clone());
        fixture
            .cleanup
            .push(CleanupResource::Docker(network.clone()));
        fixture
            .cleanup
            .push(CleanupResource::Docker(volume.clone()));
        executor.set_failed_removal_once_name(volume.name().to_owned());

        assert!(fixture.shutdown().await.is_err());
        assert_eq!(
            fixture.cleanup,
            vec![CleanupResource::Docker(volume.clone())]
        );
        assert_eq!(fixture.cleanup_errors(), &[volume.label()]);
        let removals = executor
            .calls()
            .into_iter()
            .filter(|call| {
                call.program == "docker"
                    && call.args.get(1).is_some_and(|argument| argument == "rm")
            })
            .filter_map(|call| call.args.last().cloned())
            .collect::<Vec<_>>();
        assert_eq!(
            removals,
            [volume.name().to_owned(), network.name().to_owned()]
        );

        fixture.shutdown().await?;
        assert!(fixture.cleanup.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn reporter_marks_failed_explicit_cleanup_with_cleanup_route() -> Result<()> {
        let executor = Arc::new(RecordingExecutor::default());
        let fixture_executor: Arc<dyn CommandExecutor> = executor.clone();
        let mut fixture = RegtestStack::with_executor(test_server_path(), fixture_executor)?;
        let volume = DockerResource::Volume(fixture.names.chain_volume.clone());
        fixture
            .cleanup
            .push(CleanupResource::Docker(volume.clone()));
        executor.set_failed_removal_once_name(volume.name().to_owned());

        assert!(fixture.shutdown().await.is_err());
        assert!(!fixture.shutdown_complete);
        assert_eq!(fixture.cleanup_errors(), &[volume.label()]);

        let summary = RecoveryFailureReporter::new().summary(Some(&fixture), FailureRoute::Cleanup);
        assert_eq!(summary["cleanup"], "failed");
        assert_eq!(summary["failure_route"], "cleanup");
        Ok(())
    }

    #[tokio::test]
    async fn local_cancellation_runs_explicit_reverse_order_cleanup() -> Result<()> {
        let executor = Arc::new(RecordingExecutor::default());
        let fixture_executor: Arc<dyn CommandExecutor> = executor.clone();
        let mut fixture = RegtestStack::with_executor(test_server_path(), fixture_executor)?;
        let network = DockerResource::Network(fixture.names.network.clone());
        let volume = DockerResource::Volume(fixture.names.chain_volume.clone());
        fixture
            .cleanup
            .push(CleanupResource::Docker(network.clone()));
        fixture
            .cleanup
            .push(CleanupResource::Docker(volume.clone()));
        let (sender, mut cancellation) = tokio::sync::oneshot::channel();
        sender
            .send(())
            .expect("the test-local cancellation receiver is still owned by cleanup");

        shutdown_after_test_cancellation(&mut fixture, &mut cancellation).await?;
        assert!(fixture.cleanup.is_empty());
        let removals = executor
            .calls()
            .into_iter()
            .filter(|call| {
                call.program == "docker"
                    && call.args.get(1).is_some_and(|argument| argument == "rm")
            })
            .filter_map(|call| call.args.last().cloned())
            .collect::<Vec<_>>();
        assert_eq!(
            removals,
            [volume.name().to_owned(), network.name().to_owned()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn exited_services_abort_readiness_and_recovery_without_raw_process_output() -> Result<()>
    {
        let executor = Arc::new(RecordingExecutor::default());
        executor.exited_container.store(true, Ordering::SeqCst);
        let mut fixture = RegtestStack::with_executor(test_server_path(), executor)?;

        let error = fixture
            .wait_for_node_ready()
            .await
            .expect_err("an exited node must abort readiness");
        assert!(error.to_string().contains("not running"));
        let lightwalletd = fixture.names.lightwalletd_container.clone();
        let error = fixture
            .assert_container_running_with_timeout(&lightwalletd, Duration::from_secs(1))
            .await
            .expect_err("an exited lightwalletd must abort liveness");
        assert!(error.to_string().contains("not running"));

        fixture.api_url = "http://127.0.0.1:19191".to_owned();
        fixture.server_exit_code = Some(71);
        let error = fixture
            .recovery_read::<Value>(RegtestStack::recovery_deadline(), "/api/v1/status")
            .await
            .expect_err("an exited server must abort recovery reads");
        assert!(error.to_string().contains("127.0.0.1:19191"));
        assert!(
            !error
                .to_string()
                .contains("UNIQUE_RAW_PROCESS_OUTPUT_MARKER")
        );
        fixture.shutdown().await
    }

    #[test]
    fn deadline_exhaustion_names_the_fixture_phase() {
        let error = remaining_timeout(
            Instant::now() - Duration::from_millis(1),
            Duration::from_secs(1),
        )
        .expect_err("expired fixture deadline must fail");
        assert!(
            error
                .to_string()
                .contains("fixture phase exceeded its deadline")
        );
    }

    #[test]
    fn reporter_is_available_before_setup_and_emits_only_allowlisted_fields() {
        let mut reporter = RecoveryFailureReporter::new();
        reporter.record_activity(
            "UNIQUE_TEST_SECRET_MARKER_MUST_NOT_ESCAPE",
            "unknown-status",
            "UNIQUE_TEST_SECRET_MARKER_MUST_NOT_ESCAPE",
        );
        let summary = reporter.summary(None, FailureRoute::Setup);
        let object = summary
            .as_object()
            .expect("reporter summary is always a JSON object");
        let expected = [
            "activity",
            "cleanup",
            "exit_codes",
            "failure_route",
            "heights",
            "owned_resources",
            "phase",
            "proxy_counts",
            "txid",
        ];
        assert_eq!(object.len(), expected.len());
        assert!(expected.iter().all(|key| object.contains_key(*key)));
        assert_eq!(summary["phase"], "setup");
        assert_eq!(summary["failure_route"], "setup");
        assert!(
            !summary
                .to_string()
                .contains("UNIQUE_TEST_SECRET_MARKER_MUST_NOT_ESCAPE")
        );
    }
}
