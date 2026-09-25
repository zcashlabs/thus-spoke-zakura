//! Native node lifecycle. Docker still owns the application and indexer.
use super::*;
use std::{
    cell::RefCell,
    fs::{File, OpenOptions, TryLockError},
    io::{Read, Seek, SeekFrom},
    net::TcpListener,
    path::Path,
    process::Child,
};

pub(super) struct InstanceLock(File);

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Explicitly unlock: another thread may have forked a child that has not
        // reached exec (and closed inherited file descriptors) yet.
        let _ = self.0.unlock();
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(super) enum NodeSource {
    #[default]
    Docker,
    LocalBinary {
        binary: PathBuf,
        binary_version: String,
        config: PathBuf,
        log: PathBuf,
        process: Option<ProcessIdentity>,
    },
    // External ownership, not a remote host. Keep the serialized name for compatibility.
    ExternalRpc {
        rpc: String,
        config: PathBuf,
        p2p: String,
    },
}

impl NodeSource {
    pub(super) fn description(&self) -> String {
        match self {
            Self::Docker => "Docker".into(),
            Self::LocalBinary {
                binary,
                binary_version,
                ..
            } => {
                format!("{} ({binary_version})", binary.display())
            }
            Self::ExternalRpc { rpc, .. } => format!("self-managed local Zakura at {rpc}"),
        }
    }
}

/// Includes the unique per-run configuration path and OS start time, not just a PID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ProcessIdentity {
    pid: u32,
    identity: String,
}

impl ProcessIdentity {
    fn capture(pid: u32) -> Result<Self> {
        Ok(Self {
            pid,
            identity: process_identity(pid)?.context("Zakura exited during startup")?,
        })
    }

    fn is_running(&self) -> Result<bool> {
        Ok(process_identity(self.pid)?.as_deref() == Some(self.identity.as_str()))
    }

    pub(super) fn stop(&self) -> Result<()> {
        if !self.is_running()? {
            return Ok(());
        }
        signal(self.pid, "-TERM")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.is_running()? && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        if self.is_running()? {
            signal(self.pid, "-KILL")?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.is_running()? && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        anyhow::ensure!(
            !self.is_running()?,
            "Zakura process {} did not stop; its data was retained",
            self.pid
        );
        Ok(())
    }
}

fn process_identity(pid: u32) -> Result<Option<String>> {
    let output = Command::new("ps")
        .args([
            "-ww",
            "-p",
            &pid.to_string(),
            "-o",
            "stat=",
            "-o",
            "lstart=",
            "-o",
            "args=",
        ])
        .env("LC_ALL", "C")
        .output()
        .context("checking native process identity")?;
    let value = String::from_utf8(output.stdout)?.trim().to_owned();
    if value.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        output.status.success(),
        "could not inspect native process {pid}"
    );
    // A zombie has exited, but its parent has not reaped it yet.
    let (state, identity) = value
        .split_once(char::is_whitespace)
        .context("invalid process identity")?;
    Ok((!state.starts_with('Z')).then(|| identity.trim().to_owned()))
}

fn signal(pid: u32, signal: &str) -> Result<()> {
    let output = Command::new("kill")
        .args([signal, &pid.to_string()])
        .output()?;
    if !output.status.success() && process_identity(pid)?.is_some() {
        bail!(
            "could not signal Zakura process {pid}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Networking {
    Desktop,
    Host,
}

impl Networking {
    fn discover() -> Result<Self> {
        let endpoint = match std::env::var("DOCKER_HOST") {
            Ok(value) if std::env::var_os("DOCKER_CONTEXT").is_none() => value,
            _ => docker_output([
                "context",
                "inspect",
                "--format",
                "{{.Endpoints.docker.Host}}",
            ])?,
        };
        anyhow::ensure!(
            endpoint.starts_with("unix://"),
            "local Zakura requires a local Docker daemon (remote Docker contexts are unsupported)"
        );
        let info: serde_json::Value =
            serde_json::from_str(&docker_output(["info", "--format", "{{json .}}"])?)?;
        let os = info["OperatingSystem"].as_str().unwrap_or_default();
        let rootless = info["SecurityOptions"].as_array().is_some_and(|options| {
            options
                .iter()
                .any(|v| v.as_str().is_some_and(|s| s.contains("rootless")))
        });
        Self::from_environment(os, cfg!(target_os = "linux"), rootless)
    }

    fn from_environment(os: &str, linux: bool, rootless: bool) -> Result<Self> {
        anyhow::ensure!(
            !rootless,
            "local Zakura does not yet support rootless Docker networking"
        );
        if os.contains("Docker Desktop") || os.contains("OrbStack") {
            return Ok(Self::Desktop);
        }
        if linux {
            return Ok(Self::Host);
        }
        bail!(
            "local Zakura requires Docker Desktop, OrbStack, or native Docker Engine on Linux (found {os})"
        )
    }

    fn rpc_host(self) -> &'static str {
        match self {
            Self::Desktop => "host.docker.internal",
            Self::Host => "127.0.0.1",
        }
    }

    fn network(self, prefix: &str) -> String {
        match self {
            Self::Desktop => prefix.to_owned(),
            Self::Host => "host".into(),
        }
    }
}

pub(super) struct LocalHost {
    binary: PathBuf,
    version: String,
    networking: Networking,
    child: RefCell<Option<Child>>,
    log: RefCell<Option<PathBuf>>,
}

impl LocalHost {
    pub(super) fn new(path: &Path) -> Result<Self> {
        let binary = path
            .canonicalize()
            .with_context(|| format!("finding Zakura executable {}", path.display()))?;
        anyhow::ensure!(
            binary.is_file(),
            "Zakura executable must be a file: {}",
            binary.display()
        );
        let version = binary_version(&binary)?;
        let networking = Networking::discover()?;
        Ok(Self {
            binary,
            version,
            networking,
            child: RefCell::new(None),
            log: RefCell::new(None),
        })
    }

    fn check_child(&self) -> Result<()> {
        if let Some(child) = self.child.borrow_mut().as_mut()
            && let Some(status) = child.try_wait()?
        {
            let log = self
                .log
                .borrow()
                .as_ref()
                .map(|path| log_tail(path))
                .unwrap_or_default();
            bail!("local Zakura exited ({status}):\n{log}");
        }
        Ok(())
    }

    fn stop_child(&self) -> Result<()> {
        let mut child = self.child.borrow_mut();
        if let Some(process) = child.as_mut() {
            if process.try_wait()?.is_none() {
                signal(process.id(), "-TERM")?;
                let deadline = Instant::now() + Duration::from_secs(10);
                while process.try_wait()?.is_none() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(100));
                }
                if process.try_wait()?.is_none() {
                    process.kill()?;
                }
            }
            process.wait()?;
        }
        *child = None;
        Ok(())
    }
}

impl StartHost for LocalHost {
    fn delete(&self, runtime: &Runtime, name: &InstanceName) -> Result<()> {
        // Try RPC consumers first, but always stop the node even if Docker fails.
        // Keep metadata and data if any cleanup attempt reports a failure.
        let mut failures = Vec::new();
        for service in ["app", "lightwalletd"] {
            let target = format!("{}-{service}", prefix(name));
            let result = container_exists(&target).and_then(|exists| {
                if exists {
                    docker(["rm", "-f", &target])
                } else {
                    Ok(())
                }
            });
            if let Err(error) = result {
                failures.push(format!("container {target}: {error}"));
            }
        }
        if let Err(error) = self.stop_child() {
            failures.push(format!("native Zakura: {error}"));
        }
        anyhow::ensure!(
            failures.is_empty(),
            "could not stop every instance resource; retaining development data: {}",
            failures.join("; ")
        );
        runtime.delete_instance_resources(name)
    }

    fn allocate(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
    ) -> Result<Endpoints> {
        let prefix = prefix(name);
        let dir = runtime.instance_dir(name);
        fs::create_dir_all(&dir)?;
        let native = dir.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&native)?;
        let native = native.canonicalize()?;
        let config = native.join("zakurad.toml");
        let log = native.join("zakura.log");
        *self.log.borrow_mut() = Some(log.clone());
        let mut instance = Instance {
            name: name.to_string(),
            version: 2,
            endpoints: Endpoints::default(),
            node: NodeSource::LocalBinary {
                binary: self.binary.clone(),
                binary_version: self.version.clone(),
                config: config.clone(),
                log: log.clone(),
                process: None,
            },
        };
        runtime.save_instance(name, &instance)?;
        if self.networking == Networking::Desktop {
            ensure_network(&prefix)?;
        }
        for suffix in ["wallet", "config", "lightwalletd"] {
            shutdown.check()?;
            ensure_volume(&format!("{prefix}-{suffix}"), name)?;
        }
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
        ])?;
        shutdown.check()?;
        docker(["start", "-a", &format!("{prefix}-init")])?;
        shutdown.check()?;
        docker([
            "cp",
            &format!("{prefix}-init:/config/zakurad.toml"),
            config.to_str().context("invalid config path")?,
        ])?;
        let template = fs::read_to_string(&config)?;
        // Keep both reservations until all ports have been chosen to avoid duplicates.
        let rpc_socket = TcpListener::bind("127.0.0.1:0")?;
        let p2p_socket = TcpListener::bind("127.0.0.1:0")?;
        let rpc_port = rpc_socket.local_addr()?.port();
        let p2p_port = p2p_socket.local_addr()?.port();
        fs::write(
            &config,
            native_config(&template, &native.join("chain"), rpc_port, p2p_port)?,
        )?;
        drop((rpc_socket, p2p_socket));
        println!(
            "Using local Zakura: {} ({})\n  Config: {}",
            self.binary.display(),
            self.version,
            config.display()
        );
        let output = File::create(&log)?;
        let child = Command::new(&self.binary)
            .arg("--config")
            .arg(&config)
            .arg("start")
            .current_dir(&native)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .spawn()
            .context("starting local Zakura")?;
        *self.child.borrow_mut() = Some(child);
        let pid = self.child.borrow().as_ref().expect("just spawned").id();
        let identity = ProcessIdentity::capture(pid)?;
        if let NodeSource::LocalBinary { process, .. } = &mut instance.node {
            *process = Some(identity);
        }
        runtime.save_instance(name, &instance)?;
        let rpc = format!("http://127.0.0.1:{rpc_port}");
        wait_for_rpc(&rpc, shutdown, || self.check_child())?;
        let endpoints = start_companions(
            name,
            self.networking,
            rpc_port,
            &rpc,
            &format!("127.0.0.1:{p2p_port}"),
            "local_binary",
            shutdown,
        )?;
        instance.endpoints = endpoints.clone();
        runtime.save_instance(name, &instance)?;
        Ok(endpoints)
    }

    fn wait_ready(
        &self,
        endpoints: &Endpoints,
        app: &str,
        timeout: Duration,
        shutdown: &Shutdown,
    ) -> Result<()> {
        let client = local_http_client(Duration::from_secs(3))?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            shutdown.check()?;
            self.check_child()?;
            if client
                .get(format!("{}/api/v1/health", endpoints.dashboard))
                .send()
                .is_ok_and(|r| r.status().is_success())
            {
                return Ok(());
            }
            if !container_running(app)? {
                bail!("app exited before becoming healthy:\n{}", docker_logs(app)?);
            }
            shutdown.wait_timeout(Duration::from_millis(250))?;
        }
        bail!(
            "dashboard did not become healthy within {} seconds:\n{}",
            timeout.as_secs(),
            docker_logs(app)?
        )
    }

    fn open_url(&self, url: &str) -> Result<()> {
        open_url(url)
    }

    fn wait_for_shutdown(&self, shutdown: &Shutdown) -> Result<()> {
        while !shutdown.try_interrupted() {
            self.check_child()?;
            if let Err(error) = shutdown.wait_timeout(Duration::from_millis(250))
                && !shutdown.try_interrupted()
            {
                return Err(error);
            }
        }
        Ok(())
    }
}

fn native_config(template: &str, chain: &Path, rpc_port: u16, p2p_port: u16) -> Result<String> {
    let mut config: toml::Value =
        toml::from_str(template).context("decoding generated Zakura configuration")?;
    config["rpc"]["listen_addr"] = toml::Value::String(format!("127.0.0.1:{rpc_port}"));
    config["network"]["listen_addr"] = toml::Value::String(format!("127.0.0.1:{p2p_port}"));
    config["state"]["cache_dir"] = toml::Value::String(
        chain
            .to_str()
            .context("chain directory is not UTF-8")?
            .into(),
    );
    Ok(toml::to_string_pretty(&config)?)
}

fn binary_version(binary: &Path) -> Result<String> {
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "executing {} --version (check permissions and architecture)",
                binary.display()
            )
        })?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            bail!("{} --version timed out", binary.display());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let output = child.wait_with_output()?;
    anyhow::ensure!(
        output.status.success(),
        "{} --version failed: {}",
        binary.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let version = String::from_utf8(output.stdout)?.trim().to_owned();
    anyhow::ensure!(
        !version.is_empty(),
        "{} --version returned no version",
        binary.display()
    );
    Ok(version)
}

fn wait_for_rpc(base: &str, shutdown: &Shutdown, check: impl Fn() -> Result<()>) -> Result<()> {
    let client = local_http_client(Duration::from_secs(3))?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last_error = String::new();
    while Instant::now() < deadline {
        shutdown.check()?;
        check()?;
        match client.post(base).json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getblockchaininfo","params":[]})).send() {
            Ok(response) => {
                anyhow::ensure!(!response.status().is_redirection(), "local Zakura RPC must not redirect to another endpoint");
                if response.status() == reqwest::StatusCode::UNAUTHORIZED { bail!("Zakura RPC requires authentication; use a dedicated local Regtest configuration with cookie auth disabled"); }
                let body: serde_json::Value = response.error_for_status()?.json()?;
                if let Some(chain) = body["result"]["chain"].as_str() {
                    anyhow::ensure!(chain.eq_ignore_ascii_case("regtest") || chain == "test", "expected Regtest, but Zakura reports {chain}");
                    let genesis: serde_json::Value = client.post(base).json(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"getblockhash","params":[0]})).send()?.error_for_status()?.json()?;
                    if let Some(hash) = genesis["result"].as_str() {
                        anyhow::ensure!(hash == "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327", "Zakura must use the standard Regtest genesis block");
                        return Ok(());
                    }
                    // getblockchaininfo can answer before genesis has entered the
                    // best chain. An unavailable block is not a mismatched block.
                    last_error = genesis.to_string();
                } else {
                    last_error = body.to_string();
                }
            }
            Err(error) => last_error = error.to_string(),
        }
        shutdown.wait_timeout(Duration::from_millis(250))?;
    }
    bail!("Zakura RPC {base} was not ready within 120 seconds: {last_error}")
}

fn run_docker(args: Vec<String>) -> Result<()> {
    docker_inherit(&args.iter().map(String::as_str).collect::<Vec<_>>())
}

fn start_companions(
    name: &InstanceName,
    networking: Networking,
    rpc_port: u16,
    public_rpc: &str,
    p2p: &str,
    node_mode: &str,
    shutdown: &Shutdown,
) -> Result<Endpoints> {
    let prefix = prefix(name);
    let network = networking.network(&prefix);
    let rpc_host = networking.rpc_host();
    let lwd_socket = TcpListener::bind("127.0.0.1:0")?;
    let app_socket = TcpListener::bind("127.0.0.1:0")?;
    let lwd_port = lwd_socket.local_addr()?.port();
    let app_port = app_socket.local_addr()?.port();
    let host = networking == Networking::Host;
    let mut lwd: Vec<String> = [
        "create",
        "--name",
        &format!("{prefix}-lightwalletd"),
        "--network",
        &network,
        "--label",
        &label(name),
        "--user",
        "0:0",
        "-v",
        &format!("{prefix}-lightwalletd:/var/lib/lightwalletd"),
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if !host {
        lwd.extend([
            "--network-alias".into(),
            "lightwalletd".into(),
            "-p".into(),
            "127.0.0.1::9067".into(),
        ]);
    }
    lwd.extend([
        lightwalletd_image(),
        "--no-tls-very-insecure".into(),
        "--grpc-bind-addr".into(),
        if host {
            format!("127.0.0.1:{lwd_port}")
        } else {
            "0.0.0.0:9067".into()
        },
        "--rpchost".into(),
        rpc_host.into(),
        "--rpcport".into(),
        rpc_port.to_string(),
        "--rpcuser".into(),
        "unused".into(),
        "--rpcpassword".into(),
        "unused".into(),
        "--data-dir".into(),
        "/var/lib/lightwalletd".into(),
        "--log-file".into(),
        "/dev/stdout".into(),
    ]);
    run_docker(lwd)?;
    drop(lwd_socket);
    shutdown.check()?;
    docker(["start", &format!("{prefix}-lightwalletd")])?;
    let public_lwd = format!(
        "http://127.0.0.1:{}",
        if host {
            lwd_port
        } else {
            published_port(&format!("{prefix}-lightwalletd"), "9067/tcp")?
        }
    );
    let internal_lwd = if host {
        public_lwd.clone()
    } else {
        "http://lightwalletd:9067".into()
    };
    let mut app: Vec<String> = [
        "create",
        "--name",
        &format!("{prefix}-app"),
        "--network",
        &network,
        "--label",
        &label(name),
        "-v",
        &format!("{prefix}-wallet:/data"),
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if !host {
        app.extend(["-p".into(), "127.0.0.1::8080".into()]);
    }
    for env in [
        format!(
            "TSZ_LISTEN={}",
            if host {
                format!("127.0.0.1:{app_port}")
            } else {
                "0.0.0.0:8080".into()
            }
        ),
        format!("TSZ_ZAKURA_RPC=http://{rpc_host}:{rpc_port}"),
        format!("TSZ_LIGHTWALLETD={internal_lwd}"),
        format!("TSZ_INSTANCE={name}"),
        format!("TSZ_PUBLIC_ZAKURA_RPC={public_rpc}"),
        format!("TSZ_PUBLIC_LIGHTWALLETD={public_lwd}"),
        format!("TSZ_PUBLIC_P2P={p2p}"),
        format!("TSZ_NODE_MODE={node_mode}"),
    ] {
        app.extend(["-e".into(), env]);
    }
    app.extend([
        app_image(),
        "serve".into(),
        "--data-dir".into(),
        "/data".into(),
    ]);
    run_docker(app)?;
    drop(app_socket);
    shutdown.check()?;
    docker(["start", &format!("{prefix}-app")])?;
    Ok(Endpoints {
        dashboard: format!(
            "http://127.0.0.1:{}",
            if host {
                app_port
            } else {
                published_port(&format!("{prefix}-app"), "8080/tcp")?
            }
        ),
        rpc: public_rpc.into(),
        lightwalletd: public_lwd,
        p2p: p2p.into(),
    })
}

pub(super) struct ExternalHost {
    rpc: String,
    networking: Networking,
}

impl ExternalHost {
    pub(super) fn new(runtime: &Runtime, name: &InstanceName, rpc: &str) -> Result<Self> {
        let rpc = local_rpc(rpc)?.to_string();
        let instance = runtime.read_instance(name)
            .with_context(|| format!("first run `ths --name {name} prepare --zakura-rpc {rpc}` and start Zakura using the printed configuration"))?;
        anyhow::ensure!(
            matches!(instance.node, NodeSource::ExternalRpc { rpc: ref expected, .. } if expected == &rpc),
            "instance {name} was prepared with different node settings; reset it explicitly before preparing another node"
        );
        Ok(Self {
            rpc,
            networking: Networking::discover()?,
        })
    }
}

impl StartHost for ExternalHost {
    fn external(&self) -> bool {
        true
    }
    fn prepare_start(&self, runtime: &Runtime, name: &InstanceName) -> Result<()> {
        runtime.detach_external(name)
    }
    fn delete(&self, runtime: &Runtime, name: &InstanceName) -> Result<()> {
        runtime.detach_external(name)
    }
    fn allocate(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
    ) -> Result<Endpoints> {
        let mut instance = runtime.read_instance(name)?;
        let NodeSource::ExternalRpc { rpc, p2p, config } = &instance.node else {
            bail!("instance is not prepared for an external node");
        };
        anyhow::ensure!(
            rpc == &self.rpc,
            "external node settings changed; retry start"
        );
        let port = local_rpc(rpc)?
            .port_or_known_default()
            .context("missing RPC port")?;
        println!(
            "Attaching to local Zakura at {rpc}\n  Config: {}\n  Startup will fund the development wallet and mine on this Regtest chain.",
            config.display()
        );
        wait_for_rpc(rpc, shutdown, || Ok(()))?;
        if self.networking == Networking::Desktop {
            ensure_network(&prefix(name))?;
        }
        instance.endpoints = start_companions(
            name,
            self.networking,
            port,
            rpc,
            p2p,
            "external_rpc",
            shutdown,
        )?;
        runtime.save_instance(name, &instance)?;
        Ok(instance.endpoints)
    }
    fn wait_ready(
        &self,
        endpoints: &Endpoints,
        app: &str,
        timeout: Duration,
        shutdown: &Shutdown,
    ) -> Result<()> {
        wait_ready(
            &endpoints.dashboard,
            app,
            timeout.max(Duration::from_secs(600)),
            shutdown,
        )
    }
    fn open_url(&self, url: &str) -> Result<()> {
        open_url(url)
    }
    fn wait_for_shutdown(&self, shutdown: &Shutdown) -> Result<()> {
        shutdown.wait()
    }
}

fn local_rpc(value: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(value).context("invalid --zakura-rpc URL")?;
    anyhow::ensure!(
        url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "localhost")),
        "--zakura-rpc is local-only: use http://127.0.0.1:<port> or http://localhost:<port>; internet and LAN nodes are unsupported"
    );
    anyhow::ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "--zakura-rpc must be a local RPC origin without credentials, path, query, or fragment"
    );
    url.set_host(Some("127.0.0.1"))?;
    Ok(url)
}

fn log_tail(path: &Path) -> String {
    (|| -> std::io::Result<String> {
        let mut file = File::open(path)?;
        let offset = file.metadata()?.len().saturating_sub(16_384);
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })()
    .unwrap_or_else(|error| format!("could not read {}: {error}", path.display()))
}

pub(super) fn logs(path: &Path, follow: bool) -> Result<()> {
    anyhow::ensure!(
        path.is_file(),
        "native Zakura log is unavailable: {}",
        path.display()
    );
    let mut command = Command::new("tail");
    command.args(["-n", "100"]);
    if follow {
        command.arg("-f");
    }
    let status = command
        .arg(path)
        .status()
        .context("reading native Zakura logs")?;
    anyhow::ensure!(status.success(), "reading native Zakura logs failed");
    Ok(())
}

impl Runtime {
    pub fn prepare(&self, name: &InstanceName, rpc: &str, json: bool) -> Result<()> {
        let rpc = local_rpc(rpc)?.to_string();
        let _networking = Networking::discover()?;
        let _lock = self.lock_instance(name)?;
        if self.instance_dir(name).join("instance.json").exists() {
            let instance = self.read_instance(name)?;
            anyhow::ensure!(
                matches!(instance.node, NodeSource::ExternalRpc { rpc: ref expected, .. } if expected == &rpc),
                "instance {name} already exists with other node settings; reset it explicitly first"
            );
            return print_prepared(&instance, json);
        }
        require_image(&app_image())?;
        let dir = self.instance_dir(name);
        fs::create_dir_all(&dir)?;
        // Deliberately outside instance_dir: cleanup never removes developer-owned node files.
        let node_dir = self
            .root
            .join(EXTERNAL_NODES_DIR)
            .join(name.to_string())
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&node_dir)?;
        let node_dir = node_dir.canonicalize()?;
        let config = node_dir.join("zakurad.toml");
        let p2p_socket = TcpListener::bind("127.0.0.1:0")?;
        let p2p_port = p2p_socket.local_addr()?.port();
        let rpc_port = local_rpc(&rpc)?
            .port_or_known_default()
            .context("missing RPC port")?;
        anyhow::ensure!(
            rpc_port != p2p_port,
            "RPC and P2P port collision; retry prepare"
        );
        let instance = Instance {
            name: name.to_string(),
            version: 2,
            endpoints: Endpoints::default(),
            node: NodeSource::ExternalRpc {
                rpc: rpc.clone(),
                config: config.clone(),
                p2p: format!("127.0.0.1:{p2p_port}"),
            },
        };
        let prefix = prefix(name);
        let run = |args: &[&str]| docker_command(args, None, json);
        // A failed prepare can be retried against the same wallet volume.
        for suffix in ["wallet", "config", "lightwalletd"] {
            ensure_volume_with_output(&format!("{prefix}-{suffix}"), name, json)?;
        }
        let init = format!("{prefix}-init");
        if container_exists(&init)? {
            run(&["rm", "-f", &init])?;
        }
        run(&[
            "create",
            "--name",
            &init,
            "--label",
            &label(name),
            "-v",
            &format!("{prefix}-wallet:/data"),
            "-v",
            &format!("{prefix}-config:/config"),
            &app_image(),
            "init",
            "--defer-wallet",
        ])?;
        run(&["start", "-a", &init])?;
        run(&[
            "cp",
            &format!("{init}:/config/zakurad.toml"),
            config.to_str().context("invalid config path")?,
        ])?;
        let template = fs::read_to_string(&config)?;
        fs::write(
            &config,
            native_config(&template, &node_dir.join("chain"), rpc_port, p2p_port)?,
        )?;
        self.save_instance(name, &instance)?;
        print_prepared(&instance, json)
    }

    pub(super) fn is_external(&self, name: &InstanceName) -> Result<bool> {
        if !self.instance_dir(name).join("instance.json").exists() {
            return Ok(false);
        }
        Ok(matches!(
            self.read_instance(name)?.node,
            NodeSource::ExternalRpc { .. }
        ))
    }

    pub(super) fn detach_external(&self, name: &InstanceName) -> Result<()> {
        anyhow::ensure!(
            self.is_external(name)?,
            "instance {name} is not externally managed"
        );
        let prefix = prefix(name);
        for service in ["app", "lightwalletd", "init"] {
            let target = format!("{prefix}-{service}");
            if container_exists(&target)? {
                docker(["rm", "-f", &target])?;
            }
        }
        if docker_output(["network", "inspect", &prefix]).is_ok() {
            docker(["network", "rm", &prefix])?;
        }
        Ok(())
    }

    pub(super) fn stop_request_path(&self, name: &InstanceName) -> PathBuf {
        // A concurrent stopper must not recreate files inside a directory being deleted.
        self.root.join(format!("{name}.stop-request"))
    }

    fn lock_file(&self, name: &InstanceName) -> Result<File> {
        fs::create_dir_all(&self.root)?;
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(format!("{name}.lock")))?)
    }

    pub(super) fn lock_instance(&self, name: &InstanceName) -> Result<InstanceLock> {
        let lock = self.lock_file(name)?;
        lock.try_lock().with_context(|| {
            format!("environment {name} is already running or being changed; stop it first")
        })?;
        Ok(InstanceLock(lock))
    }

    pub(super) fn stop_and_lock(&self, name: &InstanceName) -> Result<InstanceLock> {
        let lock = self.lock_file(name)?;
        let stop_request = self.stop_request_path(name);
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            match lock.try_lock() {
                Ok(()) => {
                    if stop_request.exists() {
                        fs::remove_file(&stop_request)?;
                    }
                    return Ok(InstanceLock(lock));
                }
                Err(TryLockError::WouldBlock) => {
                    fs::write(&stop_request, b"stop")?;
                    anyhow::ensure!(
                        Instant::now() < deadline,
                        "environment {name} did not stop within 45 seconds; data was retained"
                    );
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub(super) fn save_instance(&self, name: &InstanceName, instance: &Instance) -> Result<()> {
        let dir = self.instance_dir(name);
        fs::write(
            dir.join("instance.json.tmp"),
            serde_json::to_vec_pretty(instance)?,
        )?;
        fs::rename(dir.join("instance.json.tmp"), dir.join("instance.json"))?;
        Ok(())
    }
}

fn print_prepared(instance: &Instance, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(instance)?);
    } else if let NodeSource::ExternalRpc { rpc, config, .. } = &instance.node {
        println!(
            "Prepared {}.\nStart your Zakura build with --config {:?} start\nThen run: ths --name {} start --zakura-rpc {}\nThe node configuration and chain remain yours when ths stops or resets.",
            instance.name, config, instance.name, rpc
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_config_preserves_treasury_and_escapes_host_paths() {
        let template = r#"
[network]
network = "Regtest"
listen_addr = "0.0.0.0:18233"
[network.testnet_parameters.activation_heights]
NU6 = 1
[rpc]
listen_addr = "0.0.0.0:18232"
enable_cookie_auth = false
[state]
cache_dir = "/data"
[mining]
miner_address = "treasury"
"#;
        let chain = Path::new("/tmp/build with spaces/quoted\"path/chain");
        let result = native_config(template, chain, 30001, 30002).unwrap();
        let config: toml::Value = toml::from_str(&result).unwrap();
        assert_eq!(config["state"]["cache_dir"].as_str(), chain.to_str());
        assert_eq!(config["mining"]["miner_address"].as_str(), Some("treasury"));
        assert_eq!(
            config["rpc"]["listen_addr"].as_str(),
            Some("127.0.0.1:30001")
        );
        assert_eq!(
            config["network"]["listen_addr"].as_str(),
            Some("127.0.0.1:30002")
        );
        assert_eq!(
            config["network"]["testnet_parameters"]["activation_heights"]["NU6"].as_integer(),
            Some(1)
        );
    }

    #[test]
    fn local_rpc_urls_reject_remote_hosts_and_embedded_secrets() {
        assert_eq!(
            local_rpc("http://localhost:18232").unwrap().as_str(),
            "http://127.0.0.1:18232/"
        );
        for url in [
            "https://127.0.0.1:18232",
            "http://example.com",
            "http://203.0.113.1:18232",
            "http://192.168.1.10:18232",
            "http://10.0.0.5:18232",
            "http://[2001:db8::1]:18232",
            "http://localhost.example.com:18232",
            "http://127.0.0.1@example.com:18232",
            "http://0.0.0.0:18232",
            "http://user:secret@127.0.0.1",
            "http://127.0.0.1/rpc",
            "http://127.0.0.1?key=secret",
            "http://127.0.0.1/#fragment",
        ] {
            assert!(local_rpc(url).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn prepare_json_and_local_readiness_keep_their_output_and_proxy_contracts() {
        use std::{io::Write, os::unix::fs::PermissionsExt};
        const CHILD_ROOT: &str = "TSZ_TEST_PREPARE_ROOT";
        const MARKER: &str = "\nPREPARE_JSON_OUTPUT\n";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            // Run in a subprocess so proxy/PATH settings and stdout capture are isolated.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 2048];
                let _ = stream.read(&mut request).unwrap();
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .unwrap();
            });
            let (_sender, receiver) = mpsc::channel();
            wait_ready(
                &endpoint,
                "unused-app",
                Duration::from_secs(2),
                &Shutdown::from_receiver(receiver),
            )
            .unwrap();
            server.join().unwrap();
            let runtime = Runtime { root: root.into() };
            let name = "json-test".parse().unwrap();
            print!("{MARKER}");
            runtime
                .prepare(&name, "http://localhost:18232", true)
                .unwrap();
            print!("{MARKER}");
            runtime
                .prepare(&name, "http://localhost:18232", true)
                .unwrap();
            print!("{MARKER}");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let docker = dir.path().join("docker");
        fs::write(&docker, r#"#!/bin/sh
case "$1 $2" in
  'context inspect') echo 'unix:///mock-docker.sock' ;;
  'info --format') echo '{"OperatingSystem":"Docker Desktop"}' ;;
  'image inspect') exit 0 ;;
  'volume inspect'|'container inspect') exit 1 ;;
  'volume create'|'create --name') echo 'created Docker resource' ;;
  'start -a') echo 'DISPOSABLE TEST CREDENTIALS' ;;
  cp\ *) printf '[network]\nlisten_addr = "127.0.0.1:1"\n[rpc]\nlisten_addr = "127.0.0.1:2"\n[state]\ncache_dir = "/data"\n[mining]\nminer_address = "test"\n' > "$3" ;;
  *) echo "unexpected Docker command: $*" >&2; exit 1 ;;
esac
"#).unwrap();
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "runtime::local::tests::prepare_json_and_local_readiness_keep_their_output_and_proxy_contracts", "--nocapture"])
            .env(CHILD_ROOT, dir.path().join("instances"))
            .env("PATH", std::env::join_paths(std::iter::once(dir.path().to_path_buf()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))).unwrap())
            .env_remove("DOCKER_HOST").env_remove("DOCKER_CONTEXT");
        for key in [
            "http_proxy",
            "HTTP_PROXY",
            "https_proxy",
            "HTTPS_PROXY",
            "all_proxy",
            "ALL_PROXY",
        ] {
            command.env(key, "http://127.0.0.1:1");
        }
        command.env("no_proxy", "").env("NO_PROXY", "");
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let documents: Vec<_> = stdout.split(MARKER).collect();
        assert_eq!(documents.len(), 4, "{stdout}");
        let fresh: serde_json::Value = serde_json::from_str(documents[1]).unwrap();
        let repeated: serde_json::Value = serde_json::from_str(documents[2]).unwrap();
        assert_eq!(fresh, repeated);
        assert_eq!(fresh["name"], "json-test");
        assert!(!stdout.contains("DISPOSABLE TEST CREDENTIALS"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("DISPOSABLE TEST CREDENTIALS"));
    }

    #[test]
    fn remote_nodes_are_rejected_before_preparing_or_starting() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().into(),
        };
        let name: InstanceName = "local-only".parse().unwrap();
        for rpc in ["http://example.com:18232", "http://192.168.1.10:18232"] {
            let error = runtime.prepare(&name, rpc, false).unwrap_err();
            assert!(error.to_string().contains("local-only"));
            let error = ExternalHost::new(&runtime, &name, rpc).err().unwrap();
            assert!(error.to_string().contains("local-only"));
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn local_rpc_readiness_does_not_follow_http_redirects() {
        use std::io::{BufRead, BufReader, Write};
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let location = format!("http://{}", destination.local_addr().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = BufReader::new(stream.try_clone().unwrap());
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                assert_ne!(request.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    content_length = value.trim().parse::<usize>().unwrap();
                }
            }
            request.read_exact(&mut vec![0; content_length]).unwrap();
            write!(stream, "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let (_sender, receiver) = mpsc::channel();
        let error =
            wait_for_rpc(&endpoint, &Shutdown::from_receiver(receiver), || Ok(())).unwrap_err();
        server.join().unwrap();
        assert!(error.to_string().contains("must not redirect"));
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn local_rpc_readiness_retries_missing_genesis_but_rejects_wrong_genesis() {
        use std::io::{BufRead, BufReader, Write};
        for (hash, valid) in [
            (
                "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327",
                true,
            ),
            ("wrong-genesis", false),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let deadline = Instant::now() + Duration::from_secs(5);
            let server = std::thread::spawn(move || {
                let info = serde_json::json!({"result": {"chain": "test", "blocks": 0}});
                let replies = [
                    info.clone(),
                    serde_json::json!({"error": {"code": -1, "message": "No blocks in state"}}),
                    info,
                    serde_json::json!({"result": hash}),
                ];
                for reply in replies {
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                assert!(Instant::now() < deadline, "RPC request never arrived");
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            Err(error) => panic!("accepting RPC request: {error}"),
                        }
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut request = BufReader::new(stream.try_clone().unwrap());
                    let mut content_length = 0;
                    loop {
                        let mut line = String::new();
                        assert_ne!(request.read_line(&mut line).unwrap(), 0);
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':')
                            && name.eq_ignore_ascii_case("content-length")
                        {
                            content_length = value.trim().parse::<usize>().unwrap();
                        }
                    }
                    request.read_exact(&mut vec![0; content_length]).unwrap();
                    let body = reply.to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            });
            let (_sender, receiver) = mpsc::channel();
            let result = wait_for_rpc(&endpoint, &Shutdown::from_receiver(receiver), || {
                anyhow::ensure!(Instant::now() < deadline, "readiness test timed out");
                Ok(())
            });
            server.join().unwrap();
            if valid {
                result.unwrap();
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("standard Regtest genesis")
                );
            }
        }
    }

    #[test]
    fn local_modes_do_not_require_a_zakura_image() {
        assert!(
            !runtime_images(true)
                .iter()
                .any(|image| image == ZAKURA_IMAGE)
        );
        assert!(
            runtime_images(false)
                .iter()
                .any(|image| image == ZAKURA_IMAGE)
        );
    }

    #[test]
    fn supports_desktop_and_orbstack_on_linux_without_assuming_host_networking() {
        assert_eq!(
            Networking::from_environment("Docker Desktop", true, false).unwrap(),
            Networking::Desktop
        );
        assert_eq!(
            Networking::from_environment("OrbStack", false, false).unwrap(),
            Networking::Desktop
        );
        assert_eq!(
            Networking::from_environment("Ubuntu", true, false).unwrap(),
            Networking::Host
        );
        assert!(Networking::from_environment("Ubuntu", true, true).is_err());
        assert!(Networking::from_environment("Unrecognized VM", false, false).is_err());
        assert_eq!(Networking::Host.rpc_host(), "127.0.0.1");
    }

    #[test]
    fn reads_legacy_metadata_as_docker_owned() {
        let value = serde_json::json!({"name":"old","version":1,"endpoints":{"dashboard":"a","rpc":"b","lightwalletd":"c","p2p":"d"}});
        let instance: Instance = serde_json::from_value(value).unwrap();
        assert!(matches!(instance.node, NodeSource::Docker));
    }

    #[test]
    fn instance_locks_exclude_competing_starts_and_release_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().into(),
        };
        let name: InstanceName = "locking".parse().unwrap();
        let lock = runtime.lock_instance(&name).unwrap();
        assert!(runtime.lock_instance(&name).is_err());
        drop(lock);
        runtime.lock_instance(&name).unwrap();
    }

    #[test]
    fn stop_request_interrupts_readiness_without_a_signal() {
        let dir = tempfile::tempdir().unwrap();
        let (_sender, receiver) = mpsc::channel();
        let mut shutdown = Shutdown::from_receiver(receiver);
        let marker = dir.path().join("stop-request");
        shutdown.stop_request = Some(marker.clone());
        fs::write(marker, "stop").unwrap();
        assert!(shutdown.check().is_err());
        shutdown.wait().unwrap();
    }

    #[test]
    fn concurrent_stop_keeps_its_request_outside_instance_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().into(),
        };
        let name: InstanceName = "concurrent-stop".parse().unwrap();
        let instance_dir = runtime.instance_dir(&name);
        fs::create_dir_all(&instance_dir).unwrap();
        let lock = runtime.lock_instance(&name).unwrap();
        let worker_root = runtime.root.clone();
        let worker_name = name.clone();
        let worker =
            std::thread::spawn(move || Runtime { root: worker_root }.stop_and_lock(&worker_name));
        let stop_request = runtime.stop_request_path(&name);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !stop_request.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let requested = stop_request.exists();
        let outside = !stop_request.starts_with(&instance_dir);
        fs::remove_dir_all(&instance_dir).unwrap();
        // The stopper keeps requesting shutdown while the launcher holds its lock.
        std::thread::sleep(Duration::from_millis(250));
        let request_survived = stop_request.exists();
        let instance_absent = !instance_dir.exists();
        drop(lock);
        drop(worker.join().unwrap().unwrap());
        assert!(requested && outside && request_survived && instance_absent);
        assert!(
            !stop_request.exists(),
            "the stopper must clear its request after acquiring the lock"
        );
    }

    #[test]
    fn native_cleanup_stops_child_and_retains_data_when_companion_removal_fails() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD_ROOT: &str = "TSZ_TEST_CLEANUP_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let runtime = Runtime { root: root.into() };
            let name: InstanceName = "cleanup-test".parse().unwrap();
            let dir = runtime.instance_dir(&name);
            fs::create_dir_all(&dir).unwrap();
            let chain = dir.join("chain-data");
            fs::write(&chain, "retain for recovery").unwrap();
            let child = Command::new("sleep").arg("30").spawn().unwrap();
            let identity = ProcessIdentity::capture(child.id()).unwrap();
            let host = LocalHost {
                binary: "sleep".into(),
                version: "test".into(),
                networking: Networking::Host,
                child: RefCell::new(Some(child)),
                log: RefCell::new(None),
            };
            runtime
                .save_instance(
                    &name,
                    &Instance {
                        name: name.to_string(),
                        version: 2,
                        endpoints: Endpoints::default(),
                        node: NodeSource::LocalBinary {
                            binary: host.binary.clone(),
                            binary_version: host.version.clone(),
                            config: dir.join("zakurad.toml"),
                            log: dir.join("zakura.log"),
                            process: Some(identity.clone()),
                        },
                    },
                )
                .unwrap();
            let result = host.delete(&runtime, &name);
            let stopped = !identity.is_running().unwrap();
            // Reap the test process even if cleanup regresses and leaves it alive.
            host.stop_child().unwrap();
            assert!(
                stopped,
                "Docker cleanup failure left the native node running"
            );
            let failed = std::env::var("TSZ_TEST_FAIL_SERVICE").unwrap();
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains(&format!("tsz-{name}-{failed}"))
            );
            assert_eq!(fs::read_to_string(&chain).unwrap(), "retain for recovery");
            assert!(matches!(
                runtime.read_instance(&name).unwrap().node,
                NodeSource::LocalBinary { .. }
            ));
            assert_eq!(
                fs::read_to_string(runtime.root.join("removals")).unwrap(),
                "tsz-cleanup-test-app\ntsz-cleanup-test-lightwalletd\n"
            );
            return;
        }
        // Isolate the mock Docker PATH from other tests and never contact a daemon.
        let dir = tempfile::tempdir().unwrap();
        let docker = dir.path().join("docker");
        fs::write(
            &docker,
            r#"#!/bin/sh
case "$1 $2" in
  'container inspect') exit 0 ;;
  'rm -f')
    echo "$3" >> "$TSZ_TEST_CLEANUP_ROOT/removals"
    if [ "$3" = "tsz-cleanup-test-$TSZ_TEST_FAIL_SERVICE" ]; then exit 1; fi ;;
  *) echo "unexpected Docker command: $*" >&2; exit 1 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
        for service in ["app", "lightwalletd"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "runtime::local::tests::native_cleanup_stops_child_and_retains_data_when_companion_removal_fails", "--nocapture"])
                .env(CHILD_ROOT, dir.path().join(service))
                .env("TSZ_TEST_FAIL_SERVICE", service)
                .env("PATH", std::env::join_paths(std::iter::once(dir.path().to_path_buf()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))).unwrap())
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn stale_identity_never_terminates_another_process() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let actual = ProcessIdentity::capture(child.id()).unwrap();
        let stale = ProcessIdentity {
            pid: actual.pid,
            identity: "a previous process".into(),
        };
        stale.stop().unwrap();
        assert!(child.try_wait().unwrap().is_none());
        actual.stop().unwrap();
        assert!(!child.wait().unwrap().success());
    }

    #[test]
    fn log_tail_handles_large_and_non_utf8_output() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("zakura.log");
        let mut content = vec![0xff; 20_000];
        content.extend_from_slice(b"last diagnostic");
        fs::write(&log, content).unwrap();
        assert!(log_tail(&log).ends_with("last diagnostic"));
    }
}
