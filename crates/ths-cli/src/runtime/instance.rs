use super::*;

#[derive(Clone, Debug)]
pub struct InstanceName(pub(super) String);

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

pub(super) fn default_regtest() -> String {
    "regtest".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Instance {
    pub(super) name: String,
    pub(super) version: u32,
    pub(crate) endpoints: Endpoints,
    /// unix seconds; missing for environments created before it was recorded.
    #[serde(default)]
    pub(super) created_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) session: Option<String>,
}

impl Runtime {
    pub fn status(&self, name: &InstanceName, json: bool) -> Result<bool> {
        let instance = self
            .read_instance(name)
            .or_else(|_| inspect_unrecorded_instance(name, &DockerCli))?;
        let environment = inspect_environment(instance, &DockerCli)?;
        let running = environment.state == "running";
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&StatusOutput::from(&environment))?
            );
        } else {
            println!("{}", status_text(&environment));
        }
        Ok(running)
    }

    pub fn endpoints(&self, name: &InstanceName, json: bool, env: bool) -> Result<()> {
        let endpoints = self.read_instance(name)?.endpoints;
        if env {
            println!("{}", endpoint_exports(name, &endpoints));
        } else if json {
            println!("{}", serde_json::to_string_pretty(&endpoints)?);
        } else {
            println!("{}", endpoint_lines(&endpoints));
        }
        Ok(())
    }

    pub fn list(&self, json: bool) -> Result<()> {
        let environments = self.environments(&DockerCli)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&environments)?);
        } else if environments.is_empty() {
            println!("No environments yet.");
        } else {
            print!("{}", render_list(&environments, now_unix()?));
        }
        Ok(())
    }

    pub(crate) fn instance_dir(&self, name: &InstanceName) -> PathBuf {
        self.root.join(name.to_string())
    }

    pub(super) fn write_instance(&self, name: &InstanceName, endpoints: &Endpoints) -> Result<()> {
        let instance = Instance {
            name: name.to_string(),
            version: 1,
            endpoints: endpoints.clone(),
            created_at: Some(now_unix()?),
            session: self.read_session(name)?,
        };
        let path = self.instance_dir(name).join("instance.json");
        let pending = path.with_extension("json.pending");
        fs::write(&pending, serde_json::to_vec_pretty(&instance)?)?;
        fs::rename(&pending, &path)?;
        Ok(())
    }

    pub(crate) fn read_instance(&self, name: &InstanceName) -> Result<Instance> {
        let path = self.instance_dir(name).join("instance.json");
        serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("instance {name} does not exist"))?,
        )
        .context("invalid instance metadata")
    }

    pub(super) fn recovery_path(&self, name: &InstanceName) -> PathBuf {
        self.instance_dir(name).join(recovery::RECOVERY_FILE)
    }
}

fn inspect_unrecorded_instance(
    name: &InstanceName,
    docker: &impl DockerResourceCommands,
) -> Result<Instance> {
    let containers = SERVICES
        .into_iter()
        .map(|service| container_status(docker, name, service))
        .collect::<Result<Vec<_>>>()?;
    let has_owned_service = containers
        .iter()
        .any(|container| !matches!(container.state.as_str(), "missing" | "foreign"));
    if !has_owned_service {
        bail!("environment {name} is not running; start it with `ths --name {name}`");
    }
    Ok(Instance {
        name: name.to_string(),
        version: 1,
        endpoints: inspect_endpoints(&prefix(name))?,
        created_at: None,
        session: None,
    })
}

#[derive(Debug, Default)]
pub(super) struct HostPorts {
    pub(super) dashboard: u16,
    pub(super) rpc: u16,
    pub(super) p2p: u16,
    pub(super) lightwalletd: u16,
}

pub(super) fn host_ports(offset: u16) -> Result<HostPorts> {
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

pub(super) fn loopback_publish(host: u16, container: u16) -> String {
    if host == 0 {
        format!("127.0.0.1::{container}")
    } else {
        format!("127.0.0.1:{host}:{container}")
    }
}

pub(super) fn require_free_loopback(port: u16) -> Result<()> {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(_) => bail!("port {port} is already in use on 127.0.0.1"),
    }
}

pub(crate) fn prefix(name: &InstanceName) -> String {
    format!("ths-{name}")
}
pub(super) fn label(name: &InstanceName) -> String {
    format!("{INSTANCE_LABEL}={name}")
}

pub(super) fn endpoints_for(ports: &HostPorts) -> Endpoints {
    Endpoints {
        dashboard: format!("http://127.0.0.1:{}", ports.dashboard),
        rpc: format!("http://127.0.0.1:{}", ports.rpc),
        lightwalletd: format!("http://127.0.0.1:{}", ports.lightwalletd),
        p2p: format!("127.0.0.1:{}", ports.p2p),
        network: default_regtest(),
        tls: false,
    }
}

pub(super) fn endpoint_lines(e: &Endpoints) -> String {
    format!(
        "  Dashboard    {}\n  Zakura RPC   {}\n  lightwalletd {}  (network={}, tls={})\n  P2P          {}",
        e.dashboard, e.rpc, e.lightwalletd, e.network, e.tls, e.p2p
    )
}

pub(super) struct InstanceLock {
    file: File,
    path: PathBuf,
    remove_on_drop: bool,
}

fn lock_registry(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::WouldBlock) => bail!("instance lock registry is busy; try again"),
            Err(TryLockError::Error(error)) => {
                return Err(error).context("locking instance registry");
            }
        }
    }
}

impl InstanceLock {
    pub(super) fn remove_when_dropped(&mut self) {
        self.remove_on_drop = true;
    }

    pub(super) fn finish(mut self) -> Result<()> {
        self.remove_on_drop = false;
        let _registry = lock_registry(&self.path.with_file_name(".registry.lock"))?;
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("removing the instance lock"),
        }
        self.file.unlock().context("releasing the instance lock")
    }
}

/// Variables `ths run` passes to its child. `endpoints --env` prints this list.
/// `THS_NETWORK` is `regtest` and `THS_LIGHTWALLETD_TLS` is `false`.
pub(super) fn child_environment(
    name: &InstanceName,
    endpoints: &Endpoints,
) -> [(&'static str, String); 7] {
    [
        ("THS_INSTANCE", name.to_string()),
        ("THS_NETWORK", "regtest".to_owned()),
        ("THS_API_URL", endpoints.dashboard.clone()),
        ("THS_ZAKURA_RPC_URL", endpoints.rpc.clone()),
        ("THS_LIGHTWALLETD_URL", endpoints.lightwalletd.clone()),
        ("THS_LIGHTWALLETD_TLS", "false".to_owned()),
        ("THS_P2P_ADDR", endpoints.p2p.clone()),
    ]
}

pub(super) fn endpoint_exports(name: &InstanceName, endpoints: &Endpoints) -> String {
    child_environment(name, endpoints)
        .into_iter()
        .map(|(key, value)| format!("export {key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let registry = if self.remove_on_drop {
            match lock_registry(&self.path.with_file_name(".registry.lock")) {
                Ok(registry) => Some(registry),
                Err(error) => {
                    eprintln!("could not remove the instance lock: {error:#}");
                    None
                }
            }
        } else {
            None
        };
        if registry.is_some()
            && let Err(error) = fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("could not remove the instance lock: {error}");
        }
        if let Err(error) = self.file.unlock() {
            eprintln!("could not release the instance lock: {error}");
        }
    }
}

pub(super) struct StartSession {
    requested: Arc<AtomicBool>,
    stop: Option<mpsc::Sender<()>>,
    watcher: Option<thread::JoinHandle<()>>,
    _request: RemovedOnDrop,
    ready: Mutex<Option<InstanceLock>>,
    _session: SessionFile,
}
impl StartSession {
    pub(super) fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    pub(super) fn explain(&self, name: &InstanceName, error: anyhow::Error) -> anyhow::Error {
        if self.requested() {
            anyhow!("{name} was stopped by another command during startup")
        } else {
            error
        }
    }
}
impl Drop for StartSession {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(watcher) = self.watcher.take()
            && watcher.join().is_err()
        {
            eprintln!("stop request monitoring stopped unexpectedly");
        }
    }
}

struct SessionFile {
    path: PathBuf,
    id: String,
}
impl Drop for SessionFile {
    fn drop(&mut self) {
        match fs::read_to_string(&self.path) {
            Ok(id) if id.trim() == self.id => {
                if let Err(error) = remove_if_present(&self.path) {
                    eprintln!("could not remove {}: {error}", self.path.display());
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => eprintln!("could not read {}: {error}", self.path.display()),
        }
    }
}

pub(super) struct ReadyGuard<'a> {
    runtime: &'a Runtime,
    name: &'a InstanceName,
    session: &'a StartSession,
}
impl Drop for ReadyGuard<'_> {
    fn drop(&mut self) {
        self.runtime.clear_ready(self.name, self.session);
    }
}

pub(super) struct RemovedOnDrop(PathBuf);
impl Drop for RemovedOnDrop {
    fn drop(&mut self) {
        if let Err(error) = remove_if_present(&self.0) {
            eprintln!("could not remove {}: {error}", self.0.display());
        }
    }
}

fn superseded(name: &InstanceName, requested: &Option<(String, PathBuf)>) -> Result<anyhow::Error> {
    Ok(match requested {
        Some((_, request)) => {
            remove_if_present(request)?;
            anyhow!(
                "the launcher that owned {name} exited and a newer one took over; it was not stopped. Run `ths --name {name} stop` again to stop it"
            )
        }
        None => anyhow!(
            "another launcher started {name} while this command was waiting; it was not stopped. Run `ths --name {name} stop` again to stop it"
        ),
    })
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

const LOCK_PROBE_GRACE: Duration = Duration::from_secs(1);
const OWNER_POLL_GRACE: Duration = Duration::from_millis(50);

impl Runtime {
    pub(super) fn lock_instance(&self, name: &InstanceName) -> Result<InstanceLock> {
        self.acquire(&format!("{name}.lock"), LOCK_PROBE_GRACE)?
            .with_context(|| format!("environment {name} is already in use by another command"))
    }

    pub(super) fn claim_start(&self, name: &InstanceName) -> Result<InstanceLock> {
        match self.acquire(&format!("{name}.start.lock"), LOCK_PROBE_GRACE)? {
            Some(lock) => {
                let owner = self.owner_path(name);
                let pending = owner.with_extension("owner.pending");
                fs::write(&pending, uuid::Uuid::new_v4().to_string())?;
                fs::rename(&pending, &owner)?;
                self.advance_generation(name)?;
                Ok(lock)
            }
            None if self.stopping_path(name).exists() || self.stop_requested(name)? => {
                bail!("environment {name} is being stopped; start it again once that finishes")
            }
            None if self.session_path(name).exists() => bail!(
                "environment {name} is already running in another `ths start` or `ths run`; stop it first with `ths --name {name} stop`"
            ),
            None => bail!("environment {name} is already in use by another command"),
        }
    }

    pub(super) fn publish_session(
        &self,
        name: &InstanceName,
        shutdown: &Shutdown,
        poll: Duration,
    ) -> Result<StartSession> {
        let lock_dir = self.root.join(".locks");
        let stale = format!("{name}.");
        for entry in fs::read_dir(&lock_dir)? {
            let path = entry?.path();
            if path
                .file_name()
                .and_then(|file| file.to_str())
                .is_some_and(|file| file.starts_with(&stale) && file.ends_with(".stop"))
            {
                remove_if_present(&path)?;
            }
        }
        remove_if_present(&self.stopping_path(name))?;
        remove_if_present(&self.ready_path(name))?;
        let id = uuid::Uuid::new_v4().to_string();
        let session = self.session_path(name);
        let pending = lock_dir.join(format!("{name}.session.pending"));
        fs::write(&pending, &id)?;
        fs::rename(&pending, &session)?;
        let request = self.stop_request_path(name, &id);
        let (stop, stopped) = mpsc::channel();
        let requested = Arc::new(AtomicBool::new(false));
        let seen = requested.clone();
        let flag = shutdown.flag.clone();
        let signal = shutdown.signal.clone();
        let notify = shutdown.sender.clone();
        let watched = request.clone();
        let watcher = thread::spawn(move || {
            while matches!(stopped.recv_timeout(poll), Err(RecvTimeoutError::Timeout)) {
                if watched.exists() {
                    seen.store(true, Ordering::SeqCst);
                    let _ = signal.compare_exchange(
                        0,
                        signal_hook::consts::SIGINT,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    );
                    flag.store(true, Ordering::SeqCst);
                    let _ = notify.send(Ended::Interrupted);
                    return;
                }
            }
        });
        Ok(StartSession {
            requested,
            stop: Some(stop),
            watcher: Some(watcher),
            _request: RemovedOnDrop(request),
            ready: Mutex::new(None),
            _session: SessionFile { path: session, id },
        })
    }

    pub(super) fn verified_cleanup_with(
        &self,
        name: &InstanceName,
        policy: &LifecyclePolicy,
        cleanup: impl FnOnce() -> Result<CleanupReport>,
    ) -> Result<CleanupReport> {
        let deadline = Deadline::after(policy.startup_docker + policy.cleanup * 2);
        let mut requested: Option<(String, PathBuf)> = None;
        let claimed = self.read_owner(name)?;
        loop {
            if let Some(_owner) = self.acquire(&format!("{name}.start.lock"), OWNER_POLL_GRACE)? {
                if self.read_owner(name)? != claimed {
                    return Err(superseded(name, &requested)?);
                }
                self.advance_generation(name)?;
                if let Some((target, request)) = &requested {
                    remove_if_present(request)?;
                    if self.replaced_since(name, target)? {
                        bail!(
                            "the launcher that owned {name} exited and a newer one took over; it was not stopped. Run `ths --name {name} stop` again to stop it"
                        );
                    }
                }
                let _stopping = self.mark_stopping(name)?;
                remove_if_present(&self.session_path(name))?;
                let _lock = self.lock_instance(name)?;
                let mut report = cleanup()?;
                report.environment_found |= requested.is_some();
                return Ok(report);
            }
            let session = self.read_session(name)?;
            if self.read_owner(name)? != claimed {
                return Err(superseded(name, &requested)?);
            }
            match (&requested, session) {
                (None, Some(id)) => {
                    let request = self.stop_request_path(name, &id);
                    fs::write(&request, "")?;
                    println!("Asking the launcher that owns {name} to stop…");
                    requested = Some((id, request));
                }
                (Some((target, _)), Some(id)) if *target != id => bail!(
                    "the launcher that owned {name} exited and a newer one took over; it was not stopped. Run `ths --name {name} stop` again to stop it"
                ),
                _ => {}
            }
            if deadline.expired() {
                bail!("the launcher that owns {name} did not stop and clean up in time");
            }
            thread::sleep(policy.poll.min(deadline.remaining()));
        }
    }

    fn replaced_since(&self, name: &InstanceName, target: &str) -> Result<bool> {
        Ok(self.read_session(name)?.is_some_and(|id| id != target))
    }

    fn read_owner(&self, name: &InstanceName) -> Result<Option<String>> {
        match fs::read_to_string(self.owner_path(name)) {
            Ok(id) => Ok(Some(id.trim().to_owned())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("reading the environment owner"),
        }
    }

    fn owner_path(&self, name: &InstanceName) -> PathBuf {
        self.root.join(".locks").join(format!("{name}.owner"))
    }

    pub(super) fn ownership_state(
        &self,
        name: &InstanceName,
    ) -> Result<(u64, bool, Option<String>)> {
        Ok((
            self.generation(name)?,
            self.owner_active(name)?,
            self.read_session(name)?,
        ))
    }

    fn generation(&self, name: &InstanceName) -> Result<u64> {
        match fs::read_to_string(self.generation_path(name)) {
            Ok(value) => match value.trim().parse() {
                Ok(generation) => Ok(generation),
                Err(_) => Ok(0),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error).context("reading the environment generation"),
        }
    }

    fn advance_generation(&self, name: &InstanceName) -> Result<()> {
        let path = self.generation_path(name);
        let pending = path.with_extension("generation.pending");
        fs::write(&pending, (self.generation(name)? + 1).to_string())?;
        fs::rename(&pending, &path)?;
        Ok(())
    }

    fn generation_path(&self, name: &InstanceName) -> PathBuf {
        self.root.join(".locks").join(format!("{name}.generation"))
    }

    fn session_path(&self, name: &InstanceName) -> PathBuf {
        self.root.join(".locks").join(format!("{name}.session"))
    }

    fn stopping_path(&self, name: &InstanceName) -> PathBuf {
        self.root.join(".locks").join(format!("{name}.stopping"))
    }

    fn stop_request_path(&self, name: &InstanceName, session: &str) -> PathBuf {
        self.root
            .join(".locks")
            .join(format!("{name}.{session}.stop"))
    }

    pub(super) fn mark_stopping(&self, name: &InstanceName) -> Result<RemovedOnDrop> {
        let stopping = self.stopping_path(name);
        fs::write(&stopping, "")?;
        Ok(RemovedOnDrop(stopping))
    }

    pub(super) fn being_stopped(&self, name: &InstanceName) -> Result<bool> {
        Ok(
            (self.stopping_path(name).exists() && self.owner_active(name)?)
                || self.stop_requested(name)?,
        )
    }

    pub(super) fn mark_ready<'a>(
        &'a self,
        name: &'a InstanceName,
        session: &'a StartSession,
    ) -> Result<ReadyGuard<'a>> {
        (|| -> Result<()> {
            let mut ready = self
                .acquire(&format!("{name}.ready"), LOCK_PROBE_GRACE)?
                .context("another command holds the readiness lock")?;
            ready.file.set_len(0)?;
            std::io::Write::write_all(&mut ready.file, session._session.id.as_bytes())?;
            *session.ready.lock().unwrap_or_else(PoisonError::into_inner) = Some(ready);
            Ok(())
        })()
        .with_context(|| format!("recording that {name} is ready"))?;
        Ok(ReadyGuard {
            runtime: self,
            name,
            session,
        })
    }

    /// Records that `start --detach` left this session's environment running. It lives in
    /// the instance directory, so deleting the environment removes it.
    pub(super) fn mark_detached(&self, name: &InstanceName, session: &StartSession) -> Result<()> {
        let path = self.detached_path(name);
        let pending = path.with_extension("pending");
        fs::write(&pending, &session._session.id)?;
        fs::rename(&pending, &path)?;
        Ok(())
    }

    /// True when `start --detach` left this instance running, so no launcher owns it.
    pub(super) fn detached(&self, name: &InstanceName, instance: &Instance) -> Result<bool> {
        match fs::read_to_string(self.detached_path(name)) {
            Ok(id) => Ok(instance.session.as_deref() == Some(id.trim())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).context("reading the detached marker"),
        }
    }

    fn detached_path(&self, name: &InstanceName) -> PathBuf {
        self.instance_dir(name).join("detached")
    }

    fn clear_ready(&self, name: &InstanceName, session: &StartSession) {
        let ready = session
            .ready
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if ready.is_some()
            && let Err(error) = remove_if_present(&self.ready_path(name))
        {
            eprintln!(
                "could not remove {}: {error}",
                self.ready_path(name).display()
            );
        }
        drop(ready);
    }

    pub(super) fn ready_owner_wrote(
        &self,
        name: &InstanceName,
        instance: &Instance,
    ) -> Result<bool> {
        let Some(session) = self.read_session(name)? else {
            return Ok(false);
        };
        if !self.lock_held(&format!("{name}.ready"))? {
            return Ok(false);
        }
        let ready = match fs::read_to_string(self.ready_path(name)) {
            Ok(id) => id.trim() == session,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error).context("reading the launcher readiness"),
        };
        Ok(ready && instance.session.as_deref() == Some(session.as_str()))
    }

    fn ready_path(&self, name: &InstanceName) -> PathBuf {
        self.root.join(".locks").join(format!("{name}.ready"))
    }

    fn stop_requested(&self, name: &InstanceName) -> Result<bool> {
        Ok(self
            .read_session(name)?
            .is_some_and(|id| self.stop_request_path(name, &id).exists()))
    }

    fn read_session(&self, name: &InstanceName) -> Result<Option<String>> {
        match fs::read_to_string(self.session_path(name)) {
            Ok(id) if !id.trim().is_empty() => Ok(Some(id.trim().to_owned())),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("reading the launcher session"),
        }
    }

    fn acquire(&self, file_name: &str, grace: Duration) -> Result<Option<InstanceLock>> {
        let deadline = Instant::now() + grace;
        loop {
            match self.try_lock(file_name)? {
                Some(lock) => return Ok(Some(lock)),
                None if Instant::now() >= deadline => return Ok(None),
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    fn try_lock(&self, file_name: &str) -> Result<Option<InstanceLock>> {
        let lock_dir = self.root.join(".locks");
        fs::create_dir_all(&lock_dir)?;
        // Serialize opening and unlinking locks so a contender cannot lock an
        // unlinked inode while another command locks a newly created file.
        let _registry = lock_registry(&lock_dir.join(".registry.lock"))?;
        let path = lock_dir.join(file_name);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(InstanceLock {
                file,
                path,
                remove_on_drop: false,
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error).context("locking environment"),
        }
    }

    pub(super) fn owner_active(&self, name: &InstanceName) -> Result<bool> {
        self.lock_held(&format!("{name}.start.lock"))
    }

    fn lock_held(&self, file_name: &str) -> Result<bool> {
        let file = match File::open(self.root.join(".locks").join(file_name)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("opening the environment lock"),
        };
        match file.try_lock_shared() {
            Ok(()) => {
                file.unlock().context("releasing the environment lock")?;
                Ok(false)
            }
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(error)) => Err(error).context("checking the environment lock"),
        }
    }
}

impl Runtime {
    pub(super) fn environments(
        &self,
        docker: &impl DockerResourceCommands,
    ) -> Result<Vec<EnvironmentStatus>> {
        let mut environments = Vec::new();
        if self.root.exists() {
            for entry in fs::read_dir(&self.root)? {
                let path = entry?.path().join("instance.json");
                if path.exists() {
                    let instance = serde_json::from_slice::<Instance>(&fs::read(&path)?)?;
                    environments.push(inspect_environment(instance, docker)?);
                }
            }
        }
        environments.sort_by(|left, right| left.instance.name.cmp(&right.instance.name));
        Ok(environments)
    }
}

/// one container's docker state (`running`, `paused`, `exited`, ...), `missing`, or `foreign`
/// when a container this instance doesn't own holds its name, with its unix-second creation and
/// start times.
#[derive(Debug, Serialize)]
pub(super) struct ContainerStatus {
    pub(super) service: &'static str,
    pub(super) state: String,
    pub(super) created_at: Option<u64>,
    pub(super) started_at: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(super) struct EnvironmentStatus {
    #[serde(flatten)]
    pub(super) instance: Instance,
    /// `running`, `paused` (every service up, some paused), `degraded` (some services up),
    /// `stopped`, `conflict` when another owner holds one of its container names, or `unknown`
    /// when docker can't be read.
    pub(super) state: &'static str,
    pub(super) containers: Vec<ContainerStatus>,
}

#[derive(Serialize)]
pub(super) struct StatusOutput<'a> {
    name: &'a str,
    running: bool,
    state: &'static str,
    endpoints: &'a Endpoints,
    containers: &'a [ContainerStatus],
}

impl<'a> From<&'a EnvironmentStatus> for StatusOutput<'a> {
    fn from(environment: &'a EnvironmentStatus) -> Self {
        Self {
            name: &environment.instance.name,
            running: environment.state == "running",
            state: environment.state,
            endpoints: &environment.instance.endpoints,
            containers: &environment.containers,
        }
    }
}

pub(super) const SERVICES: [&str; 4] = ["app", "zakura", "lightwalletd", "init"];

fn inspect_environment(
    instance: Instance,
    docker: &impl DockerResourceCommands,
) -> Result<EnvironmentStatus> {
    let name = instance.name.parse()?;
    let containers = SERVICES
        .into_iter()
        .map(|service| container_status(docker, &name, service))
        .collect::<Result<Vec<_>>>();
    Ok(environment_status(instance, containers))
}

/// keeps an environment whose containers can't be read, such as while docker is stopped, with
/// its live status unknown.
pub(super) fn environment_status(
    instance: Instance,
    containers: Result<Vec<ContainerStatus>>,
) -> EnvironmentStatus {
    let instance = Instance {
        session: None,
        ..instance
    };
    let (state, containers) = match containers {
        Ok(containers) => (environment_state(&containers), containers),
        Err(_) => ("unknown", Vec::new()),
    };
    EnvironmentStatus {
        state,
        instance,
        containers,
    }
}

pub(super) fn status_text(environment: &EnvironmentStatus) -> String {
    format!(
        "{}: {}\n{}",
        environment.instance.name,
        environment.state,
        endpoint_lines(&environment.instance.endpoints)
    )
}

/// an environment's state from its long-running services; `init` is not one of them. a paused
/// service is still up, so it never makes an environment stopped.
pub(super) fn environment_state(containers: &[ContainerStatus]) -> &'static str {
    if containers.iter().any(|c| c.state == "foreign") {
        return "conflict";
    }
    let services: Vec<_> = containers.iter().filter(|c| c.service != "init").collect();
    let up = services
        .iter()
        .filter(|c| c.state == "running" || c.state == "paused")
        .count();
    match up {
        0 => "stopped",
        n if n < services.len() => "degraded",
        _ if services.iter().any(|c| c.state == "paused") => "paused",
        _ => "running",
    }
}

pub(super) fn container_status(
    docker: &impl DockerResourceCommands,
    name: &InstanceName,
    service: &'static str,
) -> Result<ContainerStatus> {
    let container = format!("{}-{service}", prefix(name));
    let unread = |state: &str| ContainerStatus {
        service,
        state: state.into(),
        created_at: None,
        started_at: None,
    };
    let id = match owned_resource(docker, "container", &container, name) {
        Ok(Some(id)) => id,
        Ok(None) => return Ok(unread("missing")),
        Err(error) if error.to_string().contains("is not owned by") => {
            return Ok(unread("foreign"));
        }
        Err(error) => return Err(error),
    };
    let output = docker.output(&[
        "container",
        "inspect",
        "--format",
        "{{.State.Status}} {{.Created}} {{.State.StartedAt}}",
        &id,
    ])?;
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
pub(super) fn parse_docker_time(value: &str) -> Option<u64> {
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

pub(super) fn now_unix() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before 1970")?
        .as_secs())
}

pub(super) fn ago(now: u64, then: u64) -> String {
    let seconds = now.saturating_sub(then);
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

pub(super) fn render_list(environments: &[EnvironmentStatus], now: u64) -> String {
    let (unknown, known): (Vec<_>, Vec<_>) =
        environments.iter().partition(|e| e.state == "unknown");
    let (conflict, known): (Vec<_>, Vec<_>) =
        known.into_iter().partition(|e| e.state == "conflict");
    let (active, inactive): (Vec<_>, Vec<_>) =
        known.into_iter().partition(|e| e.state != "stopped");
    let has_inactive = !inactive.is_empty();
    let mut out = String::new();
    for (title, section) in [
        ("ACTIVE", active),
        ("CONFLICT", conflict),
        ("UNKNOWN", unknown),
        ("INACTIVE", inactive),
    ] {
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
