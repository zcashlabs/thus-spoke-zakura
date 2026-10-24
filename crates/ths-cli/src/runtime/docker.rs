use super::*;

pub(super) trait DockerResourceCommands {
    fn output(&self, args: &[&str]) -> Result<String>;
    fn run(&self, args: &[&str]) -> Result<()>;
    fn create(&self, args: &[&str]) -> Result<()> {
        self.run(args)
    }
}

pub(super) struct DockerCli;

impl DockerResourceCommands for DockerCli {
    fn output(&self, args: &[&str]) -> Result<String> {
        docker_output_args(args)
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        docker_inherit(args)
    }
}

pub(super) struct LifecycleContext {
    pub(super) policy: LifecyclePolicy,
    pub(super) helpers: RefCell<HelperSet>,
    pub(super) overall_deadline: Option<Deadline>,
    pub(super) diagnostics_to_stderr: bool,
}

impl LifecycleContext {
    pub(super) fn new(policy: LifecyclePolicy) -> Self {
        Self {
            policy,
            helpers: RefCell::new(HelperSet::new()),
            overall_deadline: None,
            diagnostics_to_stderr: false,
        }
    }

    pub(super) fn new_for_start(policy: LifecyclePolicy, diagnostics_to_stderr: bool) -> Self {
        Self {
            diagnostics_to_stderr,
            ..Self::new(policy)
        }
    }

    pub(super) fn startup_deadline(&self, duration: Duration) -> Deadline {
        self.overall_deadline.map_or_else(
            || Deadline::after(duration),
            |deadline| deadline.clipped(duration),
        )
    }

    pub(super) fn finish_helpers(&self, deadline: Deadline) -> Vec<CommandFailure> {
        self.helpers.borrow_mut().finish(deadline, self.policy.poll)
    }

    pub(super) fn helpers_finished(&self) -> bool {
        self.helpers.borrow().is_empty()
    }

    pub(super) fn unresolved_helpers(&self) -> Vec<String> {
        self.helpers.borrow().unresolved_context()
    }
}

pub(super) struct LifecycleDocker<'a> {
    pub(super) context: &'a LifecycleContext,
    pub(super) enclosing_deadline: Option<Deadline>,
    pub(super) command_cap: Duration,
    pub(super) cancellation: Cancellation<'a>,
    pub(super) program: String,
    pub(super) prefix_args: Vec<String>,
    pub(super) env: Vec<(String, String)>,
    /// Test-only hook to replace how a spawned child's liveness is observed,
    /// so tests can deterministically exercise "the child looks hung" without
    /// racing a real process's actual exit timing. Always `None` outside
    /// tests.
    #[cfg(test)]
    pub(super) child_ops: Option<&'a dyn ChildOps>,
}

impl<'a> LifecycleDocker<'a> {
    pub(super) fn production(
        context: &'a LifecycleContext,
        enclosing_deadline: Option<Deadline>,
        command_cap: Duration,
        cancellation: Cancellation<'a>,
    ) -> Self {
        Self {
            context,
            enclosing_deadline,
            command_cap,
            cancellation,
            program: "docker".to_owned(),
            prefix_args: Vec::new(),
            env: Vec::new(),
            #[cfg(test)]
            child_ops: None,
        }
    }

    pub(super) fn scoped(
        &self,
        enclosing_deadline: Deadline,
        command_cap: Duration,
    ) -> LifecycleDocker<'a> {
        LifecycleDocker {
            context: self.context,
            enclosing_deadline: Some(enclosing_deadline),
            command_cap,
            cancellation: self.cancellation,
            program: self.program.clone(),
            prefix_args: self.prefix_args.clone(),
            env: self.env.clone(),
            #[cfg(test)]
            child_ops: self.child_ops,
        }
    }

    #[cfg(test)]
    pub(super) fn with_child_ops(mut self, ops: &'a dyn ChildOps) -> Self {
        self.child_ops = Some(ops);
        self
    }

    pub(super) fn operation_deadline(&self) -> Deadline {
        self.enclosing_deadline
            .map(|deadline| deadline.clipped(self.command_cap))
            .unwrap_or_else(|| Deadline::after(self.command_cap))
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = if self.program == "docker" {
            docker_cli()
        } else {
            Command::new(&self.program)
        };
        command.args(&self.prefix_args);
        command.args(args);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }

    pub(super) fn execute(
        &self,
        args: &[&str],
        mode: OutputMode,
    ) -> Result<CapturedOutput, CommandFailure> {
        let mut command = self.command(args);
        let mut helpers = self.context.helpers.borrow_mut();
        // `output()` always calls with `Capture` (reads: inspect, ls); `run()`
        // never does (mutations: rm, volume rm, network rm, volume create).
        // The injected child_ops only applies to the mutation being removed,
        // not to the read calls cleanup_core makes around it (ownership
        // preflight, absence verification) — otherwise every read would also
        // look permanently hung.
        #[cfg(test)]
        if mode != OutputMode::Capture
            && let Some(ops) = self.child_ops
        {
            return run_observed(
                &mut command,
                self.operation_deadline(),
                self.cancellation,
                mode,
                &self.context.policy,
                &mut helpers,
                ops,
                false,
            );
        }
        run(
            &mut command,
            self.operation_deadline(),
            self.cancellation,
            mode,
            &self.context.policy,
            &mut helpers,
        )
    }
}

impl DockerResourceCommands for LifecycleDocker<'_> {
    fn output(&self, args: &[&str]) -> Result<String> {
        let captured = self
            .execute(args, OutputMode::Capture)
            .map_err(|error| anyhow!("{error}"))?;
        if captured.stdout_truncated || captured.stderr_truncated {
            bail!("docker {} returned truncated output", args.join(" "));
        }
        let mut text = String::from_utf8(captured.stdout).context("docker output was not utf-8")?;
        if args.first().copied() == Some("logs") {
            text.push_str(&String::from_utf8_lossy(&captured.stderr));
        }
        Ok(text.trim().to_owned())
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        let mode = if self.context.diagnostics_to_stderr {
            OutputMode::InheritStderr
        } else {
            OutputMode::Inherit
        };
        self.execute(args, mode)?;
        Ok(())
    }

    fn create(&self, args: &[&str]) -> Result<()> {
        if self.cancellation.requested() {
            bail!(
                "docker {} was not started: shutdown was requested",
                args.join(" ")
            );
        }
        let mut command = self.command(args);
        command.stdin(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut helpers = self.context.helpers.borrow_mut();
        run_leaving_running_on_timeout(
            &mut command,
            self.operation_deadline(),
            Cancellation::Ignore,
            OutputMode::Capture,
            &self.context.policy,
            &mut helpers,
        )
        .map_err(|error| anyhow!("{error}"))?;
        Ok(())
    }
}

pub(super) fn lifecycle_docker<'a>(
    context: &'a LifecycleContext,
    deadline: Deadline,
    cancellation: Cancellation<'a>,
) -> LifecycleDocker<'a> {
    // The cleanup allowance's termination reserve stays outside every Docker
    // call so HelperSet::finish can still reap inside the original deadline.
    let operation_deadline = deadline.saturating_sub(context.policy.termination_reserve);
    LifecycleDocker::production(
        context,
        Some(operation_deadline),
        context.policy.startup_docker,
        cancellation,
    )
}

pub(super) fn docker<const N: usize>(args: [&str; N]) -> Result<()> {
    docker_inherit(&args)
}
pub(super) fn docker_inherit(args: &[&str]) -> Result<()> {
    docker_command(args, None)
}
pub(super) fn docker_inherit_in(args: &[&str], current_dir: &std::path::Path) -> Result<()> {
    docker_command(args, Some(current_dir))
}
pub(super) fn docker_command(args: &[&str], current_dir: Option<&std::path::Path>) -> Result<()> {
    #[cfg(test)]
    if let Some(docker) = scripted_docker() {
        return docker.run(args);
    }
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
pub(super) fn docker_output<const N: usize>(args: [&str; N]) -> Result<String> {
    docker_output_args(&args)
}
pub(super) fn docker_output_args(args: &[&str]) -> Result<String> {
    #[cfg(test)]
    if let Some(docker) = scripted_docker() {
        return docker.output(args);
    }
    let output = docker_cli().args(args).output().context("running Docker")?;
    if !output.status.success() {
        bail!(
            "{}",
            docker_failure_message(args, &output.status, &output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Docker usually explains itself on stderr, but it does not always: a daemon
/// killed mid-call, a signal, or a wrapper that prints nothing can exit
/// nonzero with no diagnostic at all. A message built only from stderr is then
/// empty, so every caller that reads Docker output reports a blank reason and
/// nothing identifies which probe went wrong. Name the command and its status
/// unconditionally, and append stderr only when Docker actually produced one.
pub(super) fn docker_failure_message(args: &[&str], status: &ExitStatus, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    let mut message = format!("docker {} failed ({status})", args.join(" "));
    if !stderr.is_empty() {
        message.push_str(": ");
        message.push_str(stderr);
    }
    message
}

// Tests install a Docker stand-in so a same-named container on the machine
// cannot satisfy a command that must prove ownership first.
#[cfg(test)]
thread_local! {
    static SCRIPTED_DOCKER: std::cell::RefCell<Option<std::sync::Arc<dyn DockerResourceCommands>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn scripted_docker() -> Option<std::sync::Arc<dyn DockerResourceCommands>> {
    SCRIPTED_DOCKER.with(|slot| slot.borrow().clone())
}

#[cfg(test)]
pub(super) fn with_scripted_docker<T>(
    docker: impl DockerResourceCommands + 'static,
    body: impl FnOnce() -> T,
) -> T {
    SCRIPTED_DOCKER.with(|slot| {
        *slot.borrow_mut() = Some(std::sync::Arc::new(docker));
    });
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            SCRIPTED_DOCKER.with(|slot| *slot.borrow_mut() = None);
        }
    }
    let _clear = Clear;
    body()
}

pub(super) fn docker_cli() -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new("docker");
    #[cfg(test)]
    if tests::DOCKER_UNREACHABLE.get() {
        command.env("DOCKER_HOST", "unix:///nonexistent/ths-tests/docker.sock");
    }
    // A stub program lets a test reproduce an exact Docker failure, including
    // one that prints nothing, without a daemon or a process-wide PATH change.
    #[cfg(test)]
    if let Some(program) = tests::DOCKER_PROGRAM.with(|slot| slot.borrow().clone()) {
        command = Command::new(program);
    }
    command
}
