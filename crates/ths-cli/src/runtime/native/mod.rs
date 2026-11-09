//! Native services are owned by this foreground launcher, never by persisted PIDs.
mod bundle;
mod process;
use super::*;
use process::{Processes, owns_listener};
use std::{
    io::{Read, Write},
    net::TcpListener,
};

const GENESIS: &str = "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327";

#[derive(Default, Serialize, Deserialize)]
struct NativeRecord {
    cleaned: bool,
    pids: std::collections::BTreeMap<String, u32>,
}

impl Runtime {
    pub(crate) fn native_doctor(&self, directory: Option<&Path>, json: bool) -> Result<()> {
        let bundle = bundle::resolve(directory)?;
        process::prerequisites()?;
        if json {
            println!(
                "{}",
                serde_json::json!({"ok":true,"backend":"native","config_dir":self.root,"bin_dir":bundle.bin_dir,"web_dir":bundle.web_dir})
            );
        } else {
            println!("Native runtime ready: {}", bundle.bin_dir.display());
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn native_start(
        &self,
        name: &InstanceName,
        directory: Option<&Path>,
        timeout: Duration,
        no_open: bool,
        json: bool,
        offset: u16,
        mining: MiningConfig,
        application: Option<&[OsString]>,
    ) -> Result<u8> {
        let bundle = bundle::resolve(directory)?;
        process::prerequisites()?;
        let requested_ports = if offset == 0 {
            HostPorts::default()
        } else {
            host_ports(offset)?
        };
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("startup timeout too large")?;
        let shutdown = Shutdown::install()?;
        let context = LifecycleContext::new_for_start(LifecyclePolicy::default(), true);
        let mut ownership = self.own_start(name, &shutdown, &context)?;
        ownership.remove_lifecycle_when_dropped();
        let state = self.instance_dir(name);
        fs::create_dir(&state).with_context(|| {
            format!("environment {name} already exists; choose a fresh name or stop it first")
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
        }
        // Publish the backend before any fallible initialization, including port reservation.
        fs::write(
            state.join("native.json"),
            serde_json::to_vec(&NativeRecord::default())?,
        )?;
        let mut processes = Processes::default();
        let primary = (|| -> Result<u8> {
            for dir in ["wallet", "chain", "config", "lightwalletd", "logs", "tmp"] {
                fs::create_dir(state.join(dir))?;
            }
            let mut reservations = Vec::new();
            for port in [
                requested_ports.dashboard,
                requested_ports.rpc,
                requested_ports.lightwalletd,
                requested_ports.p2p,
            ] {
                reservations.push(Some(
                    TcpListener::bind(("127.0.0.1", port))
                        .with_context(|| format!("reserving loopback port {port}"))?,
                ));
            }
            let ports: Vec<u16> = reservations
                .iter()
                .map(|socket| {
                    socket
                        .as_ref()
                        .expect("reserved")
                        .local_addr()
                        .map(|address| address.port())
                })
                .collect::<std::io::Result<_>>()?;
            let endpoints = endpoints_for(&HostPorts {
                dashboard: ports[0],
                rpc: ports[1],
                lightwalletd: ports[2],
                p2p: ports[3],
            });
            let instance = Instance {
                backend: Backend::Native,
                name: name.to_string(),
                version: 1,
                endpoints: endpoints.clone(),
                created_at: Some(now_unix()?),
                session: self.read_session(name)?,
            };
            fs::write(
                state.join("instance.json.pending"),
                serde_json::to_vec_pretty(&instance)?,
            )?;
            fs::rename(
                state.join("instance.json.pending"),
                state.join("instance.json"),
            )?;
            let mut init = service_command(&bundle.bin_dir.join("ths-server"), &state);
            init.arg("init")
                .arg("--data-dir")
                .arg(state.join("wallet"))
                .arg("--config-dir")
                .arg(state.join("config"))
                .arg("--native-chain-dir")
                .arg(state.join("chain"))
                .arg("--native-rpc-port")
                .arg(ports[1].to_string())
                .arg("--native-p2p-port")
                .arg(ports[3].to_string());
            let init_id = processes.spawn("init", &mut init, Some(&state))?;
            loop {
                check(&mut processes, &shutdown, Some(deadline))?;
                if let Some(status) = processes.status(init_id)? {
                    if !status.success() {
                        bail!("initializer exited with {status}");
                    }
                    processes.stop_one(init_id)?;
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            drop(reservations[1].take());
            drop(reservations[3].take());
            let mut node = service_command(&bundle.bin_dir.join("zakurad"), &state);
            node.arg("--config")
                .arg(state.join("config/zakurad.toml"))
                .arg("start");
            let node_id = processes.spawn("zakura", &mut node, Some(&state))?;
            processes.monitor(node_id);
            wait_listener(&mut processes, node_id, ports[1], deadline, &shutdown)?;
            wait_listener(&mut processes, node_id, ports[3], deadline, &shutdown)?;
            let client = readiness::readiness_client()?;
            loop {
                check(&mut processes, &shutdown, Some(deadline))?;
                let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getblockhash","params":[0]});
                if let Some(value) = json_probe(&client, &endpoints.rpc, Some(&body), deadline)?
                    && let Some(genesis) = value["result"].as_str()
                {
                    if genesis != GENESIS {
                        bail!("node genesis does not match Regtest");
                    }
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            drop(reservations[2].take());
            let mut indexer = service_command(&bundle.bin_dir.join("lightwalletd"), &state);
            indexer
                .args([
                    "--no-tls-very-insecure",
                    "--grpc-bind-addr",
                    &format!("127.0.0.1:{}", ports[2]),
                    "--http-bind-addr",
                    "127.0.0.1:0",
                    "--rpchost",
                    "127.0.0.1",
                    "--rpcport",
                    &ports[1].to_string(),
                    "--rpcuser",
                    "unused",
                    "--rpcpassword",
                    "unused",
                    "--log-file",
                    "/dev/stdout",
                ])
                .arg("--data-dir")
                .arg(state.join("lightwalletd"));
            let indexer_id = processes.spawn("lightwalletd", &mut indexer, Some(&state))?;
            processes.monitor(indexer_id);
            wait_listener(&mut processes, indexer_id, ports[2], deadline, &shutdown)?;
            drop(reservations[0].take());
            let mut server = service_command(&bundle.bin_dir.join("ths-server"), &state);
            server
                .arg("serve")
                .arg("--data-dir")
                .arg(state.join("wallet"))
                .env("THS_INSTANCE", name.to_string())
                .env("THS_LISTEN", format!("127.0.0.1:{}", ports[0]))
                .env("THS_ZAKURA_RPC", &endpoints.rpc)
                .env("THS_LIGHTWALLETD", &endpoints.lightwalletd)
                .env("THS_PUBLIC_ZAKURA_RPC", &endpoints.rpc)
                .env("THS_PUBLIC_LIGHTWALLETD", &endpoints.lightwalletd)
                .env("THS_PUBLIC_DASHBOARD", &endpoints.dashboard)
                .env("THS_PUBLIC_P2P", &endpoints.p2p)
                .env("THS_MINING_MODE", mining.mode.as_str())
                .env("THS_WEB_DIR", &bundle.web_dir);
            if let Some(seconds) = mining.block_time {
                server.env("THS_BLOCK_TIME", seconds.to_string());
            }
            let server_id = processes.spawn("app", &mut server, Some(&state))?;
            processes.monitor(server_id);
            wait_listener(&mut processes, server_id, ports[0], deadline, &shutdown)?;
            loop {
                check(&mut processes, &shutdown, Some(deadline))?;
                if let Some(value) = json_probe(
                    &client,
                    &format!("{}/api/v1/status", endpoints.dashboard),
                    None,
                    deadline,
                )? {
                    verify_identity(&value, name)?;
                    if value["settled"] == true {
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }
            check(&mut processes, &shutdown, Some(deadline))?;
            let record = NativeRecord {
                cleaned: false,
                pids: [
                    ("app", server_id),
                    ("zakura", node_id),
                    ("lightwalletd", indexer_id),
                ]
                .into_iter()
                .map(|(label, id)| (label.to_owned(), processes.pid(id)))
                .collect(),
            };
            fs::write(state.join("native.json"), serde_json::to_vec(&record)?)?;
            let _ready = self.mark_ready(name, ownership.session())?;
            if application.is_none() {
                if json {
                    println!("{}", serde_json::to_string(&endpoints)?);
                    std::io::stdout().flush()?;
                } else {
                    eprintln!("{name} ready\n{}", endpoint_lines(&endpoints));
                }
                if !no_open
                    && let Err(error) = open_url_with(&endpoints.dashboard, &shutdown, &context)
                    && !shutdown.try_interrupted()
                {
                    eprintln!("Warning: could not open the dashboard automatically: {error:#}");
                    eprintln!(
                        "The {name} environment is still running at {}. Run `ths --name {name} open` to open it later.",
                        endpoints.dashboard,
                    );
                }
            }
            let child = if let Some(command) = application {
                let (program, args) = command.split_first().context("run requires a command")?;
                let mut cmd = Command::new(program);
                cmd.args(args).envs(child_environment(name, &endpoints));
                Some(processes.spawn("application", &mut cmd, None)?)
            } else {
                None
            };
            loop {
                if shutdown.try_interrupted() {
                    return Ok(if ownership.session().requested() {
                        0
                    } else {
                        128 + shutdown.received_signal().unwrap_or(libc::SIGINT) as u8
                    });
                }
                check(&mut processes, &shutdown, None)?;
                if let Some(child) = child
                    && let Some(status) = processes.status(child)?
                {
                    use std::os::unix::process::ExitStatusExt;
                    return Ok(status
                        .code()
                        .map(|code| code as u8)
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1) as u8));
                }
                thread::sleep(Duration::from_millis(50));
            }
        })();
        let cleanup = processes.stop_all();
        if cleanup.is_ok() {
            if primary
                .as_ref()
                .is_ok_and(|code| *code == 0 || shutdown.try_interrupted())
            {
                fs::remove_dir_all(&state)?;
            } else {
                fs::write(
                    state.join("native.json"),
                    serde_json::to_vec(&NativeRecord {
                        cleaned: true,
                        pids: Default::default(),
                    })?,
                )?;
                eprintln!(
                    "Native processes stopped; failure logs retained at {}",
                    state.join("logs").display()
                );
            }
        }
        ownership.finish_lifecycle()?;
        match (primary, cleanup) {
            (Ok(code), Ok(())) => Ok(code),
            (Err(error), Ok(())) => Err(ownership.session().explain(name, error)),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => Err(anyhow!("{error:#}; cleanup: {cleanup:#}")),
        }
    }

    pub(crate) fn native_validate(&self, name: &InstanceName, instance: &Instance) -> Result<()> {
        self.native_validate_with_timeout(name, instance, Duration::from_secs(3))
    }

    pub(super) fn native_validate_with_timeout(
        &self,
        name: &InstanceName,
        instance: &Instance,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = Deadline::after(timeout);
        if !self.owner_active(name)?
            || !self.ready_owner_wrote(name, instance)?
            || self.being_stopped(name)?
        {
            bail!("native environment {name} has no ready owner");
        }
        let record: NativeRecord =
            serde_json::from_slice(&fs::read(self.instance_dir(name).join("native.json"))?)?;
        for (service, url) in [
            ("app", &instance.endpoints.dashboard),
            ("zakura", &instance.endpoints.rpc),
            ("lightwalletd", &instance.endpoints.lightwalletd),
        ] {
            let parsed = reqwest::Url::parse(url)?;
            if parsed.scheme() != "http" || parsed.host_str() != Some("127.0.0.1") {
                bail!("invalid native loopback endpoint");
            }
            let port = parsed.port().context("missing native endpoint port")?;
            let pid = *record
                .pids
                .get(service)
                .context("missing native process record")?;
            if !owns_listener(pid, port, deadline.clipped(Duration::from_secs(1)))? {
                bail!("native {service} does not own its recorded listener");
            }
        }
        let client = readiness::readiness_client()?;
        let status = json_probe(
            &client,
            &format!("{}/api/v1/status", instance.endpoints.dashboard),
            None,
            deadline.0,
        )?
        .context("native status unavailable")?;
        verify_identity(&status, name)?;
        if !self.ready_owner_wrote(name, instance)? || self.read_instance(name)? != *instance {
            bail!("native environment changed during validation");
        }
        Ok(())
    }

    pub(super) fn native_environment(&self, instance: Instance) -> Result<EnvironmentStatus> {
        let name = instance.name.parse()?;
        let state = if self.native_validate(&name, &instance).is_ok() {
            "running"
        } else if self.owner_active(&name)? {
            "degraded"
        } else {
            "stopped"
        };
        Ok(EnvironmentStatus {
            instance,
            state,
            containers: Vec::new(),
        })
    }

    pub(super) fn native_cleanup(&self, name: &InstanceName) -> Result<CleanupReport> {
        self.verified_cleanup_with(name,&LifecyclePolicy::default(),|| {
            let state = self.instance_dir(name);
            let found = state.exists();
            if found {
                let record: NativeRecord = serde_json::from_slice(&fs::read(state.join("native.json")).context("native cleanup was not recorded; state retained")?)?;
                if !record.cleaned { bail!("native launcher exited without verified cleanup; retained {}. Process identity is uncertain; refusing to signal persisted PIDs or delete state",state.display()); }
                fs::remove_dir_all(state)?;
            }
            Ok(CleanupReport { outcome:CleanupOutcome::VerifiedComplete, failures:Vec::new(), recovery_path:None, helpers_finished:true, environment_found:found })
        })
    }

    pub(super) fn native_logs(
        &self,
        name: &InstanceName,
        service: Option<&str>,
        follow: bool,
        tail: Option<u64>,
        head: Option<u64>,
    ) -> Result<()> {
        let path = self
            .instance_dir(name)
            .join("logs")
            .join(format!("{}.log", service.unwrap_or("app")));
        let shutdown = Shutdown::install()?;
        let mut reader = BufReader::new(File::open(path)?);
        let mut text = String::new();
        reader.read_to_string(&mut text)?;
        let lines: Vec<_> = text.lines().collect();
        let start = tail.map_or(0, |count| lines.len().saturating_sub(count as usize));
        for line in lines
            .iter()
            .skip(start)
            .take(head.unwrap_or(u64::MAX) as usize)
        {
            println!("{line}");
        }
        while follow && self.owner_active(name)? && !shutdown.try_interrupted() {
            text.clear();
            reader.read_to_string(&mut text)?;
            print!("{text}");
            std::io::stdout().flush()?;
            thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }
}

fn service_command(binary: &Path, state: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .current_dir(state)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", state)
        .env("TMPDIR", state.join("tmp"))
        .env("LANG", "C")
        .stdin(Stdio::null());
    command
}
fn check(processes: &mut Processes, shutdown: &Shutdown, deadline: Option<Instant>) -> Result<()> {
    shutdown.check()?;
    processes.check()?;
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        bail!("native startup timed out");
    }
    Ok(())
}
fn wait_listener(
    processes: &mut Processes,
    id: usize,
    port: u16,
    deadline: Instant,
    shutdown: &Shutdown,
) -> Result<()> {
    loop {
        check(processes, shutdown, Some(deadline))?;
        if owns_listener(
            processes.pid(id),
            port,
            Deadline(deadline).clipped(Duration::from_secs(1)),
        )? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(30));
    }
}
fn verify_identity(value: &serde_json::Value, name: &InstanceName) -> Result<()> {
    if value["instance"].as_str() != Some(&name.0) || value["network"] != "Regtest" {
        bail!("server instance identity or Regtest network mismatch");
    }
    Ok(())
}
fn json_probe(
    client: &reqwest::blocking::Client,
    url: &str,
    body: Option<&serde_json::Value>,
    deadline: Instant,
) -> Result<Option<serde_json::Value>> {
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_secs(1));
    if remaining.is_zero() {
        bail!("native startup timed out");
    }
    let request = match body {
        Some(body) => client.post(url).json(body),
        None => client.get(url),
    };
    let response = match request.timeout(remaining).send() {
        Ok(response) => response,
        Err(error) if error.is_timeout() || error.is_connect() => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !response.status().is_success() {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    match response.take(1024 * 1024 + 1).read_to_end(&mut bytes) {
        Ok(_) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    }
    if bytes.len() > 1024 * 1024 {
        bail!("oversized native readiness response");
    }
    if Instant::now() >= deadline {
        bail!("native startup timed out");
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}
