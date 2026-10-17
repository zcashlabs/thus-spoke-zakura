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
        }
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
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
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
    command
}
