use super::*;

pub(super) trait DockerResourceCommands {
    fn output(&self, args: &[&str]) -> Result<String>;
    fn run(&self, args: &[&str]) -> Result<()>;
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

    pub(super) fn execute(
        &self,
        args: &[&str],
        mode: OutputMode,
    ) -> Result<CapturedOutput, CommandFailure> {
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
    let output = docker_cli().args(args).output().context("running Docker")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub(super) fn docker_cli() -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new("docker");
    #[cfg(test)]
    if tests::DOCKER_UNREACHABLE.get() {
        command.env("DOCKER_HOST", "unix:///nonexistent/ths-tests/docker.sock");
    }
    command
}
