use super::*;

impl Runtime {
    pub fn start(
        &self,
        name: &InstanceName,
        no_open: bool,
        json: bool,
        port_offset: u16,
        mining: MiningConfig,
    ) -> Result<()> {
        host_ports(port_offset)?;
        let shutdown = Shutdown::install()?;
        let context = LifecycleContext::new_for_start(LifecyclePolicy::default(), json);
        let ownership = self.own_start(name, &shutdown, &context)?;
        let prepared = {
            let requested = || shutdown.try_interrupted();
            let docker = LifecycleDocker::production(
                &context,
                None,
                context.policy.startup_docker,
                Cancellation::Observe(&requested),
            );
            (|| {
                startup_docker_reachable(&docker, json)?;
                for image in [app_image(), lightwalletd_image(), ZAKURA_IMAGE.to_owned()] {
                    startup_require_image(&docker, &image)?;
                }
                Ok(())
            })()
        };
        if let Err(error) = prepared {
            let error = ownership.session().explain(name, error);
            let failures = context.finish_helpers(Deadline::after(context.policy.startup_docker));
            if !failures.is_empty() {
                return Err(anyhow!(
                    "{error:#}; helper cleanup: {}",
                    failures
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
            }
            return Err(error);
        }
        self.start_owned(
            name,
            no_open,
            json,
            port_offset,
            mining,
            &DockerHost,
            &shutdown,
            &context,
            ownership,
        )
    }
    pub fn open(&self, name: &InstanceName) -> Result<()> {
        open_url(&self.read_instance(name)?.endpoints.dashboard)
    }
}

pub(super) struct StartOwnership {
    session: Option<StartSession>,
    lifecycle: Option<InstanceLock>,
    owner: Option<InstanceLock>,
}
impl StartOwnership {
    pub(super) fn session(&self) -> &StartSession {
        self.session
            .as_ref()
            .expect("the session is held until ownership is dropped")
    }

    pub(super) fn release_lifecycle(&mut self) {
        drop(self.lifecycle.take());
    }

    pub(super) fn remove_lifecycle_when_dropped(&mut self) {
        if let Some(lifecycle) = &mut self.lifecycle {
            lifecycle.remove_when_dropped();
        }
    }

    pub(super) fn finish_lifecycle(&mut self) -> Result<()> {
        self.lifecycle.take().map_or(Ok(()), InstanceLock::finish)
    }
}
impl Drop for StartOwnership {
    fn drop(&mut self) {
        drop(self.session.take());
        drop(self.lifecycle.take());
        drop(self.owner.take());
    }
}

pub(super) struct Shutdown {
    pub(super) flag: Arc<AtomicBool>,
    pub(super) signal: Arc<AtomicI32>,
    /// Another command reported it deleted the environment. A polling caller may
    /// take the channel message before `wait` sees it, so the fact is latched
    /// here rather than carried only by the message.
    pub(super) deleted: Arc<AtomicBool>,
    /// Lets a `DeletionMonitor` end `wait` through the same channel as Ctrl+C.
    pub(super) sender: mpsc::Sender<Ended>,
    pub(super) receiver: mpsc::Receiver<Ended>,
    worker: Option<(signal_hook::iterator::Handle, thread::JoinHandle<()>)>,
}
impl Shutdown {
    #[cfg(test)]
    pub(super) fn channel() -> (mpsc::Sender<Ended>, Self) {
        let (sender, receiver) = mpsc::channel();
        let shutdown = Self {
            flag: Arc::new(AtomicBool::new(false)),
            signal: Arc::new(AtomicI32::new(0)),
            deleted: Arc::new(AtomicBool::new(false)),
            sender: sender.clone(),
            receiver,
            worker: None,
        };
        (sender, shutdown)
    }

    pub(super) fn install() -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let flag = Arc::new(AtomicBool::new(false));
        let signal = Arc::new(AtomicI32::new(0));
        let handler_flag = flag.clone();
        let handler_signal = signal.clone();
        let handler_sender = sender.clone();
        let mut signals = signal_hook::iterator::Signals::new([
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
        ])
        .context("installing shutdown signal handlers")?;
        let handle = signals.handle();
        let worker = thread::spawn(move || {
            for received in signals.forever() {
                handler_signal.store(received, Ordering::SeqCst);
                handler_flag.store(true, Ordering::SeqCst);
                if handler_sender.send(Ended::Interrupted).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            flag,
            signal,
            deleted: Arc::new(AtomicBool::new(false)),
            sender,
            receiver,
            worker: Some((handle, worker)),
        })
    }

    pub(super) fn try_interrupted(&self) -> bool {
        // Only Ctrl+C belongs to a polling caller. `recv` cannot peek, so a
        // deletion notice taken here would be lost to `wait`, which is the only
        // path that can report that another command already deleted the
        // environment. Record what was taken so the notice survives regardless.
        if let Ok(ended) = self.receiver.try_recv() {
            self.latch(ended);
        }
        self.flag.load(Ordering::SeqCst)
    }

    /// Latches a delivered shutdown reason so it survives a caller that already
    /// consumed the channel message, and returns it for the caller's own report.
    fn latch(&self, ended: Ended) -> Ended {
        match ended {
            Ended::Interrupted => {
                self.flag.store(true, Ordering::SeqCst);
            }
            Ended::DeletedElsewhere => {
                self.deleted.store(true, Ordering::SeqCst);
            }
        }
        ended
    }

    /// True once another command has reported it deleted this environment,
    /// whether the notice was delivered by `wait` or taken by a polling caller.
    pub(super) fn deleted_elsewhere(&self) -> bool {
        self.deleted.load(Ordering::SeqCst)
    }

    pub(super) fn received_signal(&self) -> Option<i32> {
        if self.try_interrupted() {
            let signal = self.signal.load(Ordering::SeqCst);
            Some(if signal == 0 {
                signal_hook::consts::SIGTERM
            } else {
                signal
            })
        } else {
            None
        }
    }

    pub(super) fn check(&self) -> Result<()> {
        if self.try_interrupted() {
            bail!("interrupted");
        }
        Ok(())
    }

    /// Waits for a shutdown signal, or for a `DeletionMonitor` to report that another command
    /// (`ths stop` or `ths reset` from another shell) deleted the environment.
    pub(super) fn wait(&self) -> Result<Ended> {
        // Not `try_interrupted`: it would take a deletion notice that is already
        // queued. Read the latches instead, so a notice a polling caller already
        // consumed still ends `wait` with the right reason.
        if self.flag.load(Ordering::SeqCst) {
            return Ok(Ended::Interrupted);
        }
        if self.deleted_elsewhere() {
            return Ok(Ended::DeletedElsewhere);
        }
        let ended = self
            .receiver
            .recv()
            .context("waiting for a shutdown signal")?;
        Ok(self.latch(ended))
    }

    pub(super) fn wait_timeout(&self, timeout: Duration) -> Result<()> {
        if self.try_interrupted() {
            bail!("interrupted");
        }
        if self.deleted_elsewhere() {
            bail!("{DELETED_ELSEWHERE}");
        }
        match self.receiver.recv_timeout(timeout) {
            Ok(ended) => match self.latch(ended) {
                Ended::Interrupted => bail!("interrupted"),
                Ended::DeletedElsewhere => bail!("{DELETED_ELSEWHERE}"),
            },
            Err(RecvTimeoutError::Timeout) => Ok(()),
            Err(RecvTimeoutError::Disconnected) => {
                if self.try_interrupted() {
                    bail!("interrupted");
                }
                if self.deleted_elsewhere() {
                    bail!("{DELETED_ELSEWHERE}");
                }
                Err(anyhow!("waiting for a shutdown signal"))
            }
        }
    }
}

impl Drop for Shutdown {
    fn drop(&mut self) {
        if let Some((handle, worker)) = self.worker.take() {
            handle.close();
            if worker.join().is_err() {
                eprintln!("shutdown signal worker panicked");
            }
        }
    }
}

const DELETED_ELSEWHERE: &str = "the environment was deleted by another command";

/// How a running environment's foreground `start` ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ended {
    /// Ctrl+C or a termination signal: `start` deletes the environment itself.
    Interrupted,
    /// Another command already deleted it.
    DeletedElsewhere,
}

pub(super) trait StartHost {
    fn instance_exists(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        _shutdown: &Shutdown,
        _context: &LifecycleContext,
    ) -> Result<bool> {
        Ok(runtime.instance_dir(name).exists())
    }
    fn runtime_status(&self, endpoints: &Endpoints, timeout: Duration) -> Result<RuntimeStatus> {
        runtime_status(endpoints, timeout)
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
    fn diagnostics(&self, _name: &InstanceName, _context: &LifecycleContext) -> String {
        String::new()
    }
    fn delete(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        context: &LifecycleContext,
        deadline: Deadline,
    ) -> Result<CleanupReport>;
    fn delete_partial(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        context: &LifecycleContext,
        deadline: Deadline,
    ) -> Result<CleanupReport> {
        self.delete(runtime, name, context, deadline)
    }
    #[allow(clippy::too_many_arguments)]
    fn allocate(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
        port_offset: Option<u16>,
        mining: MiningConfig,
        context: &LifecycleContext,
        journal: &mut recovery::RecoveryJournal,
    ) -> Result<AllocatedInstance>;
    fn wait_ready(
        &self,
        allocated: &AllocatedInstance,
        shutdown: &Shutdown,
        context: &LifecycleContext,
    ) -> Result<()>;
    fn open_url(&self, url: &str, shutdown: &Shutdown, context: &LifecycleContext) -> Result<()>;
    fn wait_for_shutdown(
        &self,
        app_container: &WatchedContainer,
        shutdown: &Shutdown,
    ) -> Result<Ended>;
}

pub(super) struct DockerHost;

impl StartHost for DockerHost {
    fn diagnostics(&self, name: &InstanceName, context: &LifecycleContext) -> String {
        let deadline = Deadline::after(Duration::from_secs(5));
        let docker = LifecycleDocker::production(
            context,
            Some(deadline),
            context.policy.readiness_docker,
            Cancellation::Ignore,
        );
        ["app", "zakura", "lightwalletd"]
            .map(|service| {
                let container = format!("{}-{service}", prefix(name));
                let logs = readiness::diagnostic_logs(&docker, &container)
                    .unwrap_or_else(|error| format!("could not read logs: {error:#}"));
                format!("\n--- {service} ---\n{logs}")
            })
            .join("")
    }

    fn instance_exists(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
        context: &LifecycleContext,
    ) -> Result<bool> {
        if runtime.instance_dir(name).exists() {
            return Ok(true);
        }
        let requested = || shutdown.try_interrupted();
        let docker = LifecycleDocker::production(
            context,
            context.overall_deadline,
            context.policy.startup_docker,
            Cancellation::Observe(&requested),
        );
        let label_filter = format!("label={}", label(name));
        let network_name = format!("name=^{}$", prefix(name));
        for args in [
            vec![
                "container",
                "ls",
                "--all",
                "--filter",
                &label_filter,
                "--format",
                "{{.ID}}",
            ],
            vec![
                "volume",
                "ls",
                "--filter",
                &label_filter,
                "--format",
                "{{.Name}}",
            ],
            vec![
                "network",
                "ls",
                "--filter",
                &network_name,
                "--format",
                "{{.Name}}",
            ],
        ] {
            if !docker.output(&args)?.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn delete(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        context: &LifecycleContext,
        deadline: Deadline,
    ) -> Result<CleanupReport> {
        let docker = lifecycle_docker(context, deadline, Cancellation::Ignore);
        runtime.cleanup_core(name, &docker, false, deadline, context)
    }

    fn delete_partial(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        context: &LifecycleContext,
        deadline: Deadline,
    ) -> Result<CleanupReport> {
        let docker = lifecycle_docker(context, deadline, Cancellation::Ignore);
        runtime.cleanup_core(name, &docker, true, deadline, context)
    }

    #[allow(clippy::too_many_arguments)]
    fn allocate(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
        port_offset: Option<u16>,
        mining: MiningConfig,
        context: &LifecycleContext,
        journal: &mut recovery::RecoveryJournal,
    ) -> Result<AllocatedInstance> {
        let requested = || shutdown.try_interrupted();
        let docker = LifecycleDocker::production(
            context,
            context.overall_deadline,
            context.policy.startup_docker,
            Cancellation::Observe(&requested),
        );
        fs::create_dir_all(runtime.instance_dir(name))?;
        shutdown.check()?;
        let prefix = prefix(name);
        let mut ports = port_offset.map(host_ports).transpose()?.unwrap_or_default();
        for port in [ports.dashboard, ports.rpc, ports.p2p, ports.lightwalletd] {
            if port != 0 {
                require_free_loopback(port)?;
            }
        }
        shutdown.check()?;
        let network_budget = context.startup_deadline(context.policy.startup_docker);
        let network = ensure_network_with(
            &prefix,
            name,
            &docker.scoped(network_budget, context.policy.startup_docker),
            journal,
            network_budget,
        )?;
        shutdown.check()?;
        let mut volumes = Vec::new();
        for suffix in ["chain", "wallet", "lightwalletd", "config"] {
            let budget = context.startup_deadline(context.policy.startup_docker);
            volumes.push(ensure_volume_with(
                &format!("{prefix}-{suffix}"),
                name,
                &docker.scoped(budget, context.policy.startup_docker),
                journal,
                budget,
            )?);
            shutdown.check()?;
        }
        let init_name = format!("{prefix}-init");
        let init_budget = context.startup_deadline(context.policy.startup_docker);
        let init_docker = docker.scoped(init_budget, context.policy.startup_docker);
        if owned_resource(&init_docker, "container", &init_name, name)?.is_none() {
            let app = app_image();
            let init_id = create_container(
                &init_name,
                name,
                &init_docker,
                journal,
                init_budget,
                volumes.clone(),
                &[
                    "create",
                    "--name",
                    &init_name,
                    "--label",
                    &label(name),
                    "-v",
                    &format!("{prefix}-wallet:/data"),
                    "-v",
                    &format!("{prefix}-config:/config"),
                    &app,
                    "init",
                    "--data-dir",
                    "/data",
                    "--config-dir",
                    "/config",
                ],
            )?;
            shutdown.check()?;
            let start_budget = context.startup_deadline(context.policy.initialization);
            let starter = docker.scoped(start_budget, context.policy.initialization);
            let index = journal.begin(
                recovery::ResourceRef {
                    kind: recovery::ResourceKind::Container,
                    name: init_name.clone(),
                    identity: Some(init_id.clone()),
                },
                volumes.clone(),
                recovery::MutationOperation::Start,
            )?;
            if let Err(error) = starter.run(&["start", "-a", &init_id]) {
                return Err(mutation_failure(journal, index, error, Some(init_id)));
            }
            journal.finish(
                index,
                recovery::MutationOutcome::Acknowledged,
                Some(init_id),
            )?;
            shutdown.check()?;
        }
        let zakura_budget = context.startup_deadline(context.policy.startup_docker);
        let zakura_id = ensure_zakura(
            &prefix,
            name,
            &ports,
            &docker.scoped(zakura_budget, context.policy.startup_docker),
            journal,
            zakura_budget,
            vec![network.clone()],
        )?;
        shutdown.check()?;
        let lightwalletd_budget = context.startup_deadline(context.policy.startup_docker);
        let lightwalletd_id = ensure_lightwalletd(
            &prefix,
            name,
            &ports,
            &docker.scoped(lightwalletd_budget, context.policy.startup_docker),
            journal,
            lightwalletd_budget,
            vec![network.clone()],
        )?;
        shutdown.check()?;
        let zakura_start = context.startup_deadline(context.policy.startup_docker);
        start_container(
            &zakura_id,
            &format!("{prefix}-zakura"),
            &docker.scoped(zakura_start, context.policy.startup_docker),
            journal,
            zakura_start,
        )?;
        shutdown.check()?;
        let zakura_rpc = format!(
            "http://127.0.0.1:{}",
            published_port_with(
                &docker.scoped(zakura_start, context.policy.startup_docker),
                &zakura_id,
                "18232/tcp",
            )?
        );
        let tip_deadline = context.startup_deadline(context.policy.readiness);
        readiness::wait_for_zakura_tip(
            &zakura_rpc,
            &zakura_id,
            tip_deadline,
            shutdown,
            &context.policy,
            &docker.scoped(tip_deadline, context.policy.readiness_docker),
        )?;
        let lightwalletd_start = context.startup_deadline(context.policy.startup_docker);
        start_container(
            &lightwalletd_id,
            &format!("{prefix}-lightwalletd"),
            &docker.scoped(lightwalletd_start, context.policy.startup_docker),
            journal,
            lightwalletd_start,
        )?;
        shutdown.check()?;
        let app_budget = context.startup_deadline(context.policy.startup_docker);
        if port_offset.is_none() {
            let docker = docker.scoped(app_budget, context.policy.startup_docker);
            ports.rpc = published_port_with(&docker, &zakura_id, "18232/tcp")?;
            ports.p2p = published_port_with(&docker, &zakura_id, "18233/tcp")?;
            ports.lightwalletd = published_port_with(&docker, &lightwalletd_id, "9067/tcp")?;
        }
        let app_container = ensure_app(
            &prefix,
            name,
            &ports,
            &docker.scoped(app_budget, context.policy.startup_docker),
            journal,
            app_budget,
            vec![network],
            mining,
        )?;
        shutdown.check()?;
        let app_start = context.startup_deadline(context.policy.startup_docker);
        start_container(
            &app_container.id,
            &format!("{prefix}-app"),
            &docker.scoped(app_start, context.policy.startup_docker),
            journal,
            app_start,
        )?;
        shutdown.check()?;
        let endpoints = if port_offset.is_none() {
            inspect_endpoints_with(
                &docker.scoped(app_start, context.policy.startup_docker),
                &app_container.id,
                &zakura_id,
                &lightwalletd_id,
            )?
        } else {
            endpoints_for(&ports)
        };
        runtime.write_instance(name, &endpoints)?;
        Ok(AllocatedInstance {
            endpoints,
            resources: journal.record().resources.clone(),
            app_container,
            zakura_container_id: zakura_id,
        })
    }

    fn wait_ready(
        &self,
        allocated: &AllocatedInstance,
        shutdown: &Shutdown,
        context: &LifecycleContext,
    ) -> Result<()> {
        let requested = || shutdown.try_interrupted();
        let deadline = context.startup_deadline(context.policy.readiness);
        let docker = LifecycleDocker::production(
            context,
            Some(deadline),
            context.policy.readiness_docker,
            Cancellation::Observe(&requested),
        );
        readiness::wait_ready(
            &allocated.endpoints.dashboard,
            &allocated.app_container.id,
            deadline,
            shutdown,
            &context.policy,
            &docker,
        )
    }

    fn open_url(&self, url: &str, shutdown: &Shutdown, context: &LifecycleContext) -> Result<()> {
        open_url_with(url, shutdown, context)
    }

    fn wait_for_shutdown(
        &self,
        app_container: &WatchedContainer,
        shutdown: &Shutdown,
    ) -> Result<Ended> {
        let _monitor = DeletionMonitor::spawn(
            Arc::new(DockerEvents::default()),
            app_container,
            DELETION_WATCH_RETRY,
            shutdown,
        );
        shutdown.wait()
    }
}

pub(super) enum CleanupGuard {
    Inactive,
    Armed,
    Explicit,
}

pub(super) struct CleanupOnDrop<'a> {
    pub(super) runtime: &'a Runtime,
    pub(super) name: &'a InstanceName,
    pub(super) host: &'a dyn StartHost,
    pub(super) context: &'a LifecycleContext,
    pub(super) state: CleanupGuard,
}

impl Drop for CleanupOnDrop<'_> {
    fn drop(&mut self) {
        if !matches!(self.state, CleanupGuard::Armed) {
            return;
        }
        let deadline = Deadline::after(self.context.policy.cleanup);
        if let Err(error) =
            self.host
                .delete_partial(self.runtime, self.name, self.context, deadline)
        {
            eprintln!("could not delete {}: {error:#}", self.name);
        }
    }
}

impl Runtime {
    #[cfg(test)]
    pub(super) fn start_with(
        &self,
        name: &InstanceName,
        no_open: bool,
        json: bool,
        port_offset: u16,
        host: &dyn StartHost,
        shutdown: &Shutdown,
    ) -> Result<()> {
        let context = LifecycleContext::new(LifecyclePolicy::default());
        self.start_with_policy(name, no_open, json, port_offset, host, shutdown, &context)
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    pub(super) fn start_with_policy(
        &self,
        name: &InstanceName,
        no_open: bool,
        json: bool,
        port_offset: u16,
        host: &dyn StartHost,
        shutdown: &Shutdown,
        context: &LifecycleContext,
    ) -> Result<()> {
        host_ports(port_offset)?;
        let ownership = self.own_start(name, shutdown, context)?;
        self.start_owned(
            name,
            no_open,
            json,
            port_offset,
            MiningConfig::default(),
            host,
            shutdown,
            context,
            ownership,
        )
    }

    pub(super) fn own_start(
        &self,
        name: &InstanceName,
        shutdown: &Shutdown,
        context: &LifecycleContext,
    ) -> Result<StartOwnership> {
        let owner = self.claim_start(name)?;
        let lifecycle = Some(self.lock_instance(name)?);
        let session = self.publish_session(name, shutdown, context.policy.poll)?;
        Ok(StartOwnership {
            session: Some(session),
            lifecycle,
            owner: Some(owner),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn start_owned(
        &self,
        name: &InstanceName,
        no_open: bool,
        json: bool,
        port_offset: u16,
        mining: MiningConfig,
        host: &dyn StartHost,
        shutdown: &Shutdown,
        context: &LifecycleContext,
        mut ownership: StartOwnership,
    ) -> Result<()> {
        let mut cleanup = CleanupOnDrop {
            runtime: self,
            name,
            host,
            context,
            state: CleanupGuard::Inactive,
        };
        print_progress(json, format_args!("Preparing a fresh {name} environment…"));
        let initial_deadline = Deadline::after(context.policy.cleanup);
        let initial = host.delete(self, name, context, initial_deadline)?;
        if !initial.verified() {
            return initial.into_result();
        }
        cleanup.state = CleanupGuard::Armed;
        print_progress(json, format_args!("Starting {name}…"));
        let primary = (|| {
            fs::create_dir_all(self.instance_dir(name))?;
            let mut journal = recovery::RecoveryJournal::create(self.recovery_path(name), name)?;
            let allocated = host.allocate(
                self,
                name,
                shutdown,
                Some(port_offset),
                mining,
                context,
                &mut journal,
            )?;
            if allocated.zakura_container_id.is_empty()
                || allocated
                    .resources
                    .iter()
                    .any(|resource| resource.name.is_empty())
            {
                bail!("startup did not record the allocated resource identities");
            }
            shutdown.check()?;
            host.wait_ready(&allocated, shutdown, context)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&allocated.endpoints)?);
            } else {
                println!(
                    "\n{name} is ready 🌸\n{}",
                    endpoint_lines(&allocated.endpoints)
                );
            }
            ownership.release_lifecycle();
            if !no_open
                && let Err(error) = host.open_url(&allocated.endpoints.dashboard, shutdown, context)
                && !shutdown.try_interrupted()
            {
                eprintln!("Warning: could not open the dashboard automatically: {error:#}");
                eprintln!(
                    "The {name} environment is still running at {}. Run `ths --name {name} open` to open it later.",
                    allocated.endpoints.dashboard,
                );
            }
            if !json {
                println!("\nPress Ctrl+C to stop and delete this development environment.");
            }
            let ready = self.mark_ready(name, ownership.session())?;
            let ended = host.wait_for_shutdown(&allocated.app_container, shutdown);
            drop(ready);
            ended
        })();
        cleanup.state = CleanupGuard::Explicit;
        let primary = primary.map_err(|error| ownership.session().explain(name, error));
        if matches!(primary, Ok(Ended::DeletedElsewhere)) {
            print_progress(
                json,
                format_args!("\n{name} was stopped and deleted by another command."),
            );
            return Ok(());
        }
        let _stopping = match self.mark_stopping(name) {
            Ok(marker) => Some(marker),
            Err(error) => {
                eprintln!("could not mark {name} as stopping: {error:#}");
                None
            }
        };
        if primary.is_ok() {
            print_progress(json, format_args!("\nStopping and deleting {name}…"));
        }
        let final_deadline = Deadline::after(context.policy.cleanup);
        let cleaned = host
            .delete_partial(self, name, context, final_deadline)
            .and_then(CleanupReport::into_result);
        match (primary, cleaned) {
            (Ok(_), Ok(())) => {
                print_progress(
                    json,
                    format_args!("Deleted {name} and all of its development data."),
                );
                Ok(())
            }
            (Err(primary), Ok(())) => Err(primary),
            (Ok(_), Err(error)) => Err(error),
            (Err(primary), Err(error)) => Err(anyhow!("{primary:#}\n{error:#}")),
        }
    }
}

pub(super) fn startup_docker_reachable(
    docker: &impl DockerResourceCommands,
    diagnostics_to_stderr: bool,
) -> Result<()> {
    let version = docker
        .output(&["version", "--format", "{{.Server.Version}}"])
        .context("Docker is not reachable; start Docker Desktop or the Docker daemon")?;
    print_progress(diagnostics_to_stderr, format_args!("✓ Docker {version}"));
    Ok(())
}

pub(super) fn startup_require_image(docker: &LifecycleDocker<'_>, image: &str) -> Result<()> {
    match docker.execute(
        &["image", "inspect", "--format", "{{.Id}}", image],
        OutputMode::Capture,
    ) {
        Ok(output) if !output.stdout_truncated && !output.stderr_truncated => Ok(()),
        Ok(_) => bail!("checking image {image} returned truncated output"),
        Err(error) if error.kind == CommandFailureKind::Nonzero => {
            confirm_image_absent(docker, image, &error.stderr)
        }
        Err(error) => Err(anyhow!("{error}").context(format!("checking image {image}"))),
    }
}

pub(super) fn confirm_image_absent(
    docker: &LifecycleDocker<'_>,
    image: &str,
    inspect_stderr: &[u8],
) -> Result<()> {
    match docker.execute(
        &["image", "ls", "--format", "{{.Repository}}:{{.Tag}}"],
        OutputMode::Capture,
    ) {
        Ok(listing) if !listing.stdout_truncated => {
            let present = String::from_utf8_lossy(&listing.stdout)
                .lines()
                .any(|line| line.trim() == image);
            if present {
                Ok(())
            } else {
                bail!(
                    "required image {image} is unavailable; run `ths pull` (or `ths build` from a source checkout) first"
                )
            }
        }
        Ok(listing) => bail!(
            "checking image {image} failed: {}; image listing was not usable: {}",
            String::from_utf8_lossy(inspect_stderr).trim(),
            String::from_utf8_lossy(&listing.stderr).trim()
        ),
        Err(error) => Err(anyhow!("{error}").context(format!(
            "checking image {image} failed: {}",
            String::from_utf8_lossy(inspect_stderr).trim()
        ))),
    }
}

pub(super) fn open_url_with(
    url: &str,
    shutdown: &Shutdown,
    context: &LifecycleContext,
) -> Result<()> {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else {
        ("xdg-open", vec![url])
    };
    let requested = || shutdown.try_interrupted();
    let mut command = Command::new(program);
    command.args(args);
    let deadline = Deadline::after(context.policy.startup_docker);
    let mut helpers = context.helpers.borrow_mut();
    run(
        &mut command,
        deadline,
        Cancellation::Observe(&requested),
        OutputMode::Null,
        &context.policy,
        &mut helpers,
    )
    .map_err(|error| anyhow!("{error}"))
    .with_context(|| format!("opening {url}"))?;
    drop(helpers);
    shutdown.check()?;
    Ok(())
}
pub(super) fn open_url(url: &str) -> Result<()> {
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
