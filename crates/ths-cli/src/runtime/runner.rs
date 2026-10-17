use super::*;

impl Runtime {
    pub fn wait(&self, name: &InstanceName, timeout: Duration, json: bool) -> Result<()> {
        let shutdown = Shutdown::install()?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("settlement timeout is too large")?;
        let status = self.wait_for_settlement(
            name,
            deadline,
            STARTUP_GRACE,
            &shutdown,
            !json,
            &runtime_status,
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

    pub(super) fn wait_for_settlement(
        &self,
        name: &InstanceName,
        deadline: Instant,
        grace: Duration,
        shutdown: &Shutdown,
        progress: bool,
        read_status: &dyn Fn(&Endpoints, Duration) -> Result<RuntimeStatus>,
    ) -> Result<RuntimeStatus> {
        let grace = Instant::now() + grace;
        let seen = std::cell::Cell::new(false);
        let metadata = self.instance_dir(name).join("instance.json");
        let read_instance = || match self.read_instance(name) {
            Ok(instance) => {
                seen.set(true);
                Ok(Some(instance))
            }
            Err(error) if metadata.exists() => Err(error),
            Err(_) => Ok(None),
        };
        let current = |instance: &Instance| -> Result<()> {
            if self.being_stopped(name)? {
                bail!("environment {name} is being stopped");
            }
            if !self.owner_active(name)? {
                if Instant::now() < grace {
                    bail!("environment {name} has no running launcher yet");
                }
                return Err(anyhow!(WaitEnded(format!(
                    "environment {name} has no running launcher; its containers were left by one that exited. Delete them with `ths --name {name} stop`"
                ))));
            }
            if !self.ready_owner_wrote(name, instance)? {
                bail!("environment {name} is still starting");
            }
            Ok(())
        };
        let observe = |instance: Instance, remaining: Duration| -> Result<RuntimeStatus> {
            let ownership = self.ownership_state(name)?;
            current(&instance)?;
            let status = read_status(&instance.endpoints, remaining)?;
            if self.ownership_state(name)? != ownership {
                bail!("environment {name} changed owner while its status was read");
            }
            match read_instance()? {
                Some(after) if after == instance => current(&after)?,
                Some(_) => bail!("environment {name} changed while its status was read"),
                None => bail!("environment {name} was deleted while its status was read"),
            }
            Ok(status)
        };
        wait_until_settled(
            &|remaining| {
                if let Some(instance) = read_instance()? {
                    return observe(instance, remaining);
                }
                if !seen.get() && Instant::now() < grace {
                    bail!("environment {name} does not exist yet");
                }
                if self.owner_active(name)? {
                    if seen.get() {
                        bail!("environment {name} is being replaced or deleted");
                    }
                    bail!("environment {name} is still starting");
                }
                if let Some(instance) = read_instance()? {
                    return observe(instance, remaining);
                }
                Err(anyhow!(WaitEnded(if seen.get() {
                    format!("environment {name} was deleted while waiting")
                } else {
                    format!("instance {name} does not exist")
                })))
            },
            deadline,
            Duration::from_millis(750),
            shutdown,
            progress,
        )
    }

    pub fn run(&self, name: &InstanceName, timeout: Duration, command: &[OsString]) -> Result<u8> {
        let shutdown = Shutdown::install()?;
        let mut context = LifecycleContext::new(LifecyclePolicy::default());
        let deadline = Deadline(
            Instant::now()
                .checked_add(timeout)
                .context("startup timeout is too large")?,
        );
        context.overall_deadline = Some(deadline);
        host_ports(0)?;
        let ownership = self.own_start(name, &shutdown, &context)?;
        let prepared = {
            let requested = || shutdown.try_interrupted();
            let docker = LifecycleDocker::production(
                &context,
                Some(deadline),
                context.policy.startup_docker,
                Cancellation::Observe(&requested),
            );
            (|| {
                startup_docker_reachable(&docker, false)?;
                for image in [app_image(), lightwalletd_image(), ZAKURA_IMAGE.to_owned()] {
                    startup_require_image(&docker, &image)?;
                }
                Ok(())
            })()
        };
        if let Err(error) = prepared {
            let error = ownership.session().explain(name, error);
            let failures = context.finish_helpers(Deadline::after(context.policy.cleanup));
            if failures.is_empty() {
                return Err(error);
            }
            return Err(anyhow!(
                "{error:#}; helper cleanup: {}",
                failures
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        self.run_owned(
            name,
            command,
            &std::env::current_dir().context("reading the current directory")?,
            &DockerHost,
            &shutdown,
            &context,
            ownership,
        )
    }
    #[cfg(test)]
    pub(super) fn run_with_policy(
        &self,
        name: &InstanceName,
        command: &[OsString],
        current_dir: &Path,
        host: &dyn StartHost,
        shutdown: &Shutdown,
        context: &LifecycleContext,
    ) -> Result<u8> {
        host_ports(0)?;
        let ownership = self.own_start(name, shutdown, context)?;
        self.run_owned(
            name,
            command,
            current_dir,
            host,
            shutdown,
            context,
            ownership,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_owned(
        &self,
        name: &InstanceName,
        command: &[OsString],
        current_dir: &Path,
        host: &dyn StartHost,
        shutdown: &Shutdown,
        context: &LifecycleContext,
        ownership: StartOwnership,
    ) -> Result<u8> {
        let session = ownership.session();
        let deadline = context.overall_deadline.context("missing run deadline")?;
        if deadline.expired() {
            bail!("environment did not start before the timeout");
        }
        if host.instance_exists(self, name, shutdown, context)? {
            bail!("environment {name} already exists");
        }
        if deadline.expired() {
            bail!("environment did not start before the timeout");
        }
        fs::create_dir_all(&self.root)?;
        fs::create_dir(self.instance_dir(name))
            .with_context(|| format!("reserving fresh environment {name}"))?;
        let mut cleanup = CleanupOnDrop {
            runtime: self,
            name,
            host,
            context,
            state: CleanupGuard::Armed,
        };
        println!("Preparing a fresh {name} environment…");
        let primary = (|| {
            let mut journal = recovery::RecoveryJournal::create(self.recovery_path(name), name)?;
            let allocated = host.allocate(self, name, shutdown, 0, context, &mut journal)?;
            if allocated.zakura_container_id.is_empty()
                || allocated
                    .resources
                    .iter()
                    .any(|resource| resource.name.is_empty())
            {
                bail!("startup did not record the allocated resource identities");
            }
            shutdown.check()?;
            if deadline.expired() {
                bail!("environment did not start before the timeout");
            }
            host.wait_ready(&allocated, shutdown, context)?;
            shutdown.check()?;
            if deadline.expired() {
                bail!("environment did not become ready before the timeout");
            }
            wait_until_settled(
                &|remaining| host.runtime_status(&allocated.endpoints, remaining),
                deadline.0,
                Duration::from_millis(750),
                shutdown,
                true,
            )?;
            println!("{name} is settled; running child command…");
            let ready = self.mark_ready(name, session)?;
            let outcome =
                host.run_child(command, current_dir, name, &allocated.endpoints, shutdown);
            drop(ready);
            outcome
        })();
        if matches!(&primary, Ok(ChildOutcome::Exited(code)) if *code != 0)
            || matches!(&primary, Ok(ChildOutcome::Signaled))
        {
            let diagnostics = host.diagnostics(name, context);
            if !diagnostics.is_empty() {
                eprintln!("Run diagnostics:{diagnostics}");
            }
        }
        cleanup.state = CleanupGuard::Explicit;
        let primary = primary.map_err(|error| session.explain(name, error));
        if session.requested() && matches!(primary, Ok(ChildOutcome::Signaled)) {
            eprintln!("{name} was stopped by another command.");
        }
        let _stopping = match self.mark_stopping(name) {
            Ok(marker) => Some(marker),
            Err(error) => {
                eprintln!("could not mark {name} as stopping: {error:#}");
                None
            }
        };
        let cleaned = host
            .delete_partial(self, name, context, Deadline::after(context.policy.cleanup))
            .and_then(CleanupReport::into_result);
        match (primary, cleaned) {
            (Ok(ChildOutcome::Exited(code)), Ok(())) => Ok(code),
            (Ok(ChildOutcome::Signaled), Ok(())) => Ok(1),
            (Err(primary), Ok(())) => Err(primary),
            (Ok(ChildOutcome::Exited(0)), Err(cleanup)) => Err(cleanup),
            (Ok(ChildOutcome::Exited(code)), Err(cleanup)) => {
                eprintln!("could not delete {name}: {cleanup:#}");
                Ok(code)
            }
            (Ok(ChildOutcome::Signaled), Err(cleanup)) => {
                eprintln!("could not delete {name}: {cleanup:#}");
                Ok(1)
            }
            (Err(primary), Err(cleanup)) => Err(anyhow!("{primary:#}\n{cleanup:#}")),
        }
    }
}

pub(super) const STARTUP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(super) struct WaitEnded(String);

impl Display for WaitEnded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WaitEnded {}

pub(super) fn display_height(height: Option<u64>) -> String {
    height.map_or_else(|| "unavailable".to_owned(), |height| height.to_string())
}

pub(super) fn wait_until_settled(
    read_status: &dyn Fn(Duration) -> Result<RuntimeStatus>,
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
        let status = match read_status(remaining.min(Duration::from_secs(5))) {
            Ok(status) => status,
            Err(error) if error.is::<WaitEnded>() => return Err(error),
            Err(error) => {
                let error = format!("{error:#}");
                if progress && last_error.as_deref() != Some(error.as_str()) {
                    eprintln!("Waiting: runtime status unavailable ({error})");
                }
                last_error = Some(error);
                shutdown.wait_timeout(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(poll_interval),
                )?;
                continue;
            }
        };
        last_error = None;
        if Instant::now() >= deadline {
            continue;
        }
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
        shutdown.wait_timeout(
            deadline
                .saturating_duration_since(Instant::now())
                .min(poll_interval),
        )?;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChildOutcome {
    Exited(u8),
    Signaled,
}

pub(super) fn run_child(
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
    process.args(arguments).current_dir(current_dir);
    for (key, value) in child_environment(name, endpoints) {
        process.env(key, value);
    }
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
pub(super) fn terminate_child_group(
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
            Ok(()) | Err(Errno::EPERM) => {}
            Err(error) => return Err(error).context("checking child process group"),
        }
        if Instant::now() >= deadline {
            killpg(pgid, Signal::SIGKILL)
                .or_else(|error| (error == Errno::ESRCH).then_some(()).ok_or(error))
                .context("force-terminating child process group")?;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if !reaped {
        child.wait().context("reaping child command")?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn terminate_child_group(
    child: &mut std::process::Child,
    _signal: i32,
    _grace: Duration,
) -> Result<()> {
    child.kill().context("terminating child command")?;
    child.wait().context("reaping child command")?;
    Ok(())
}
