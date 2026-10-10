//! Absolute deadlines, cancellation, and finite helper supervision.
//!
//! A clipped deadline is the earlier of the original instant and `now + cap`.
//! Clipping never moves that original instant forward. The termination reserve
//! is subtracted from an operation allowance and never extends it.

use std::{
    fmt::{self, Display},
    io::{self, Read},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Deadline(pub(crate) Instant);

impl Deadline {
    pub(crate) fn after(duration: Duration) -> Self {
        Self(Instant::now() + duration)
    }

    pub(crate) fn remaining(self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }

    pub(crate) fn expired(self) -> bool {
        self.remaining().is_zero()
    }

    /// Earlier of this deadline and `now + cap`. The stored origin instant is unchanged
    /// when `cap` extends past it, so a later clip observes time already spent.
    pub(crate) fn clipped(self, cap: Duration) -> Self {
        Self(self.0.min(Instant::now() + cap))
    }

    pub(crate) fn saturating_sub(self, duration: Duration) -> Self {
        match self.0.checked_sub(duration) {
            Some(instant) => Self(instant),
            None => Self(Instant::now()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LifecyclePolicy {
    pub(crate) readiness: Duration,
    pub(crate) http_attempt: Duration,
    pub(crate) readiness_docker: Duration,
    pub(crate) startup_docker: Duration,
    pub(crate) initialization: Duration,
    pub(crate) cleanup: Duration,
    pub(crate) poll: Duration,
    pub(crate) termination_reserve: Duration,
    pub(crate) machine_output_limit: usize,
    pub(crate) diagnostic_output_limit: usize,
    pub(crate) http_body_limit: usize,
}

impl Default for LifecyclePolicy {
    fn default() -> Self {
        Self {
            readiness: Duration::from_secs(120),
            http_attempt: Duration::from_secs(2),
            readiness_docker: Duration::from_secs(2),
            startup_docker: Duration::from_secs(30),
            initialization: Duration::from_secs(120),
            cleanup: Duration::from_secs(120),
            poll: Duration::from_millis(25),
            termination_reserve: Duration::from_millis(250),
            machine_output_limit: 1024 * 1024,
            diagnostic_output_limit: 64 * 1024,
            http_body_limit: 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Cancellation<'a> {
    Observe(&'a dyn Fn() -> bool),
    Ignore,
}

impl Cancellation<'_> {
    pub(crate) fn requested(self) -> bool {
        match self {
            Self::Observe(is_requested) => is_requested(),
            Self::Ignore => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputMode {
    Inherit,
    Capture,
    /// Discard both streams. Used by the startup browser launcher, which must not
    /// inherit the launcher's terminal or retain browser-helper output.
    Null,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandFailureKind {
    Cancelled,
    TimedOut,
    Spawn,
    Nonzero,
    Capture,
    Termination,
    Reaping,
}

impl Display for CommandFailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed out",
            Self::Spawn => "spawn failed",
            Self::Nonzero => "nonzero exit",
            Self::Capture => "capture failed",
            Self::Termination => "termination failed",
            Self::Reaping => "reaping failed",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapturedOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
}

#[derive(Debug)]
pub(crate) struct CommandFailure {
    pub(crate) kind: CommandFailureKind,
    pub(crate) command: String,
    pub(crate) detail: String,
    /// Retained for diagnostics. Production formatting uses `stderr`.
    #[allow(dead_code)]
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) helper_reaped: bool,
}

impl Display for CommandFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} ({})", self.command, self.kind, self.detail)?;
        if !self.stderr.is_empty() {
            write!(
                f,
                "; stderr: {}",
                String::from_utf8_lossy(&self.stderr).trim()
            )?;
        }
        if !self.helper_reaped {
            write!(f, "; helper was not reaped")?;
        }
        Ok(())
    }
}

impl std::error::Error for CommandFailure {}

struct PendingHelper {
    child: Child,
    command: String,
}

pub(crate) struct HelperSet {
    pending: Vec<PendingHelper>,
}

impl HelperSet {
    pub(crate) fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(crate) fn unresolved_context(&self) -> Vec<String> {
        self.pending
            .iter()
            .map(|helper| helper.command.clone())
            .collect()
    }

    /// Kills and reaps retained helpers until `deadline`. Unresolved children stay in the set.
    pub(crate) fn finish(&mut self, deadline: Deadline, poll: Duration) -> Vec<CommandFailure> {
        let pending = std::mem::take(&mut self.pending);
        let mut failures = Vec::new();
        for mut helper in pending {
            match terminate_and_reap(&mut helper.child, deadline, poll, &RealChildOps) {
                Ok(()) => {}
                Err(detail) => {
                    failures.push(CommandFailure {
                        kind: CommandFailureKind::Reaping,
                        command: helper.command.clone(),
                        detail,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                        helper_reaped: false,
                    });
                    self.pending.push(helper);
                }
            }
        }
        failures
    }
}

pub(crate) trait ChildOps {
    fn kill(&self, child: &mut Child) -> io::Result<()>;
    fn try_wait(&self, child: &mut Child) -> io::Result<Option<ExitStatus>>;
}

struct RealChildOps;

impl ChildOps for RealChildOps {
    fn kill(&self, child: &mut Child) -> io::Result<()> {
        child.kill()
    }

    fn try_wait(&self, child: &mut Child) -> io::Result<Option<ExitStatus>> {
        child.try_wait()
    }
}

pub(crate) fn run(
    command: &mut Command,
    deadline: Deadline,
    cancellation: Cancellation<'_>,
    mode: OutputMode,
    policy: &LifecyclePolicy,
    helpers: &mut HelperSet,
) -> Result<CapturedOutput, CommandFailure> {
    run_observed(
        command,
        deadline,
        cancellation,
        mode,
        policy,
        helpers,
        &RealChildOps,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_observed(
    command: &mut Command,
    deadline: Deadline,
    cancellation: Cancellation<'_>,
    mode: OutputMode,
    policy: &LifecyclePolicy,
    helpers: &mut HelperSet,
    ops: &dyn ChildOps,
    fail_nonblocking: bool,
) -> Result<CapturedOutput, CommandFailure> {
    let command_name = command_label(command);
    if cancellation.requested() || deadline.expired() {
        return Err(failure(
            if cancellation.requested() {
                CommandFailureKind::Cancelled
            } else {
                CommandFailureKind::TimedOut
            },
            &command_name,
            "the operation budget was already exhausted",
            Vec::new(),
            Vec::new(),
            true,
        ));
    }

    match mode {
        OutputMode::Inherit => {
            command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        }
        OutputMode::Capture => {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        OutputMode::Null => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Err(failure(
                CommandFailureKind::Spawn,
                &command_name,
                &error.to_string(),
                Vec::new(),
                Vec::new(),
                true,
            ));
        }
    };

    if mode == OutputMode::Capture {
        let setup = if fail_nonblocking {
            Err(io::Error::other("nonblocking pipe setup failed"))
        } else {
            configure_stdio(&mut child)
        };
        if let Err(error) = setup {
            let reaped = match terminate_and_reap(&mut child, deadline, policy.poll, ops) {
                Ok(()) => true,
                Err(detail) => {
                    helpers.pending.push(PendingHelper {
                        child,
                        command: command_name.clone(),
                    });
                    return Err(failure(
                        CommandFailureKind::Capture,
                        &command_name,
                        &format!("{error}; {detail}"),
                        Vec::new(),
                        Vec::new(),
                        false,
                    ));
                }
            };
            return Err(failure(
                CommandFailureKind::Capture,
                &command_name,
                &error.to_string(),
                Vec::new(),
                Vec::new(),
                reaped,
            ));
        }
    }

    let reserve = policy.termination_reserve.min(deadline.remaining());
    let execution = deadline.saturating_sub(reserve);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut stdout_truncated = false;
    let mut stderr_truncated = false;
    let mut capture_error: Option<String> = None;

    loop {
        if mode == OutputMode::Capture {
            drain_child(
                &mut child,
                &mut stdout,
                &mut stderr,
                policy,
                &mut stdout_truncated,
                &mut stderr_truncated,
                &mut capture_error,
            );
        }
        if let Some(mut error) = capture_error.take() {
            if let Err(kill_error) = ops.kill(&mut child) {
                error.push_str(&format!("; kill failed: {kill_error}"));
            }
            return retain_or_fail(
                child,
                command_name,
                CommandFailureKind::Capture,
                error,
                stdout,
                stderr,
                deadline,
                policy.poll,
                helpers,
                ops,
            );
        }

        match ops.try_wait(&mut child) {
            Ok(Some(status)) => {
                return finish_exited(
                    child,
                    status,
                    command_name,
                    mode,
                    deadline,
                    cancellation,
                    policy,
                    stdout,
                    stderr,
                    stdout_truncated,
                    stderr_truncated,
                    helpers,
                    ops,
                );
            }
            Ok(None) => {}
            Err(error) => {
                helpers.pending.push(PendingHelper {
                    child,
                    command: command_name.clone(),
                });
                return Err(failure(
                    CommandFailureKind::Reaping,
                    &command_name,
                    &error.to_string(),
                    stdout,
                    stderr,
                    false,
                ));
            }
        }

        if cancellation.requested() || execution.expired() {
            let kind = if cancellation.requested() {
                CommandFailureKind::Cancelled
            } else {
                CommandFailureKind::TimedOut
            };
            if let Err(error) = ops.kill(&mut child) {
                helpers.pending.push(PendingHelper {
                    child,
                    command: command_name.clone(),
                });
                return Err(failure(
                    CommandFailureKind::Termination,
                    &command_name,
                    &format!("{kind}; {error}"),
                    stdout,
                    stderr,
                    false,
                ));
            }
            return retain_or_fail(
                child,
                command_name,
                kind,
                kind.to_string(),
                stdout,
                stderr,
                deadline,
                policy.poll,
                helpers,
                ops,
            );
        }

        let pause = policy.poll.min(execution.remaining());
        if !pause.is_zero() {
            thread::sleep(pause);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_exited(
    mut child: Child,
    status: ExitStatus,
    command_name: String,
    mode: OutputMode,
    deadline: Deadline,
    cancellation: Cancellation<'_>,
    policy: &LifecyclePolicy,
    mut stdout: Vec<u8>,
    mut stderr: Vec<u8>,
    mut stdout_truncated: bool,
    mut stderr_truncated: bool,
    helpers: &mut HelperSet,
    ops: &dyn ChildOps,
) -> Result<CapturedOutput, CommandFailure> {
    if mode == OutputMode::Capture {
        let mut capture_error = None;
        let settled = drain_until_eof(
            &mut child,
            &mut stdout,
            &mut stderr,
            policy,
            &mut stdout_truncated,
            &mut stderr_truncated,
            &mut capture_error,
            deadline,
            cancellation,
        );
        if let Some(error) = capture_error {
            return retain_or_fail(
                child,
                command_name,
                CommandFailureKind::Capture,
                error,
                stdout,
                stderr,
                deadline,
                policy.poll,
                helpers,
                ops,
            );
        }
        if !settled {
            drop(child.stdout.take());
            drop(child.stderr.take());
            let cancelled = cancellation.requested();
            return Err(failure(
                if cancelled {
                    CommandFailureKind::Cancelled
                } else {
                    CommandFailureKind::Capture
                },
                &command_name,
                if cancelled {
                    "cancelled while collecting output after the direct child exited"
                } else {
                    "stdout or stderr stayed open after the direct child exited"
                },
                stdout,
                stderr,
                true,
            ));
        }
    }
    if cancellation.requested() {
        return Err(failure(
            CommandFailureKind::Cancelled,
            &command_name,
            "cancelled after the direct child exited",
            stdout,
            stderr,
            true,
        ));
    }
    if status.success() {
        Ok(CapturedOutput {
            status,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        })
    } else {
        Err(failure(
            CommandFailureKind::Nonzero,
            &command_name,
            &format!("exit status {status}"),
            stdout,
            stderr,
            true,
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn retain_or_fail(
    mut child: Child,
    command_name: String,
    kind: CommandFailureKind,
    detail: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    deadline: Deadline,
    poll: Duration,
    helpers: &mut HelperSet,
    ops: &dyn ChildOps,
) -> Result<CapturedOutput, CommandFailure> {
    match reap_until(&mut child, deadline, poll, ops) {
        Ok(()) => Err(failure(kind, &command_name, &detail, stdout, stderr, true)),
        Err(reap_detail) => {
            helpers.pending.push(PendingHelper {
                child,
                command: command_name.clone(),
            });
            Err(failure(
                kind,
                &command_name,
                &format!("{detail}; {reap_detail}"),
                stdout,
                stderr,
                false,
            ))
        }
    }
}

fn terminate_and_reap(
    child: &mut Child,
    deadline: Deadline,
    poll: Duration,
    ops: &dyn ChildOps,
) -> Result<(), String> {
    let kill = ops.kill(child);
    match reap_until(child, deadline, poll, ops) {
        Ok(()) => Ok(()),
        Err(reap_error) => match kill {
            Ok(()) => Err(reap_error),
            Err(kill_error) => Err(format!("kill failed: {kill_error}; {reap_error}")),
        },
    }
}

fn reap_until(
    child: &mut Child,
    deadline: Deadline,
    poll: Duration,
    ops: &dyn ChildOps,
) -> Result<(), String> {
    loop {
        match ops.try_wait(child) {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if deadline.expired() => {
                return Err(
                    "the child was still running when the operation deadline expired".into(),
                );
            }
            Ok(None) => {
                let pause = poll.min(deadline.remaining());
                if pause.is_zero() {
                    return Err(
                        "the child was still running when the operation deadline expired".into(),
                    );
                }
                thread::sleep(pause);
            }
            Err(error) => return Err(format!("try_wait failed: {error}")),
        }
    }
}

fn configure_stdio(child: &mut Child) -> io::Result<()> {
    if let Some(pipe) = child.stdout.as_ref() {
        nonblocking(pipe)?;
    }
    if let Some(pipe) = child.stderr.as_ref() {
        nonblocking(pipe)?;
    }
    Ok(())
}

/// Sets `O_NONBLOCK` on a pipe inherited from `spawn` without dropping existing flags.
///
/// The descriptor is borrowed from the child pipe for both `fcntl` calls and is not closed here.
#[cfg(unix)]
fn nonblocking(pipe: &impl std::os::fd::AsFd) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = pipe.as_fd().as_raw_fd();
    // The borrowed descriptor remains valid throughout both calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn nonblocking(_pipe: &impl std::os::fd::AsFd) -> io::Result<()> {
    Err(io::Error::other(
        "nonblocking child pipes require a Unix launcher target",
    ))
}

const READ_CHUNK: usize = 8 * 1024;
const READS_PER_TURN: usize = 16;

fn drain_child(
    child: &mut Child,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    policy: &LifecyclePolicy,
    stdout_truncated: &mut bool,
    stderr_truncated: &mut bool,
    capture_error: &mut Option<String>,
) {
    if let Some(pipe) = child.stdout.as_mut()
        && let Err(error) = drain_one(pipe, stdout, policy.machine_output_limit, stdout_truncated)
    {
        *capture_error = Some(format!("stdout: {error}"));
    }
    if let Some(pipe) = child.stderr.as_mut()
        && let Err(error) = drain_one(
            pipe,
            stderr,
            policy.diagnostic_output_limit,
            stderr_truncated,
        )
    {
        *capture_error = Some(format!("stderr: {error}"));
    }
}

/// Reads until both pipes report EOF. Returns false when the deadline expires first.
#[allow(clippy::too_many_arguments)]
fn drain_until_eof(
    child: &mut Child,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    policy: &LifecyclePolicy,
    stdout_truncated: &mut bool,
    stderr_truncated: &mut bool,
    capture_error: &mut Option<String>,
    deadline: Deadline,
    cancellation: Cancellation<'_>,
) -> bool {
    loop {
        if cancellation.requested() {
            return false;
        }
        let mut stdout_eof = child.stdout.is_none();
        let mut stderr_eof = child.stderr.is_none();
        if let Some(pipe) = child.stdout.as_mut() {
            match drain_one(pipe, stdout, policy.machine_output_limit, stdout_truncated) {
                Ok(true) => stdout_eof = true,
                Ok(false) => {}
                Err(error) => {
                    *capture_error = Some(format!("stdout: {error}"));
                    return false;
                }
            }
        }
        if let Some(pipe) = child.stderr.as_mut() {
            match drain_one(
                pipe,
                stderr,
                policy.diagnostic_output_limit,
                stderr_truncated,
            ) {
                Ok(true) => stderr_eof = true,
                Ok(false) => {}
                Err(error) => {
                    *capture_error = Some(format!("stderr: {error}"));
                    return false;
                }
            }
        }
        if stdout_eof && stderr_eof {
            return true;
        }
        if deadline.expired() {
            return false;
        }
        let pause = policy.poll.min(deadline.remaining());
        if pause.is_zero() {
            return false;
        }
        thread::sleep(pause);
    }
}

/// Returns `Ok(true)` on EOF. Past the retained-output cap, bytes are discarded and the stream
/// is still read so a noisy child cannot stall the supervisor.
fn drain_one(
    pipe: &mut impl Read,
    retained: &mut Vec<u8>,
    limit: usize,
    truncated: &mut bool,
) -> io::Result<bool> {
    let mut chunk = [0u8; READ_CHUNK];
    for _ in 0..READS_PER_TURN {
        match pipe.read(&mut chunk) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                let room = limit.saturating_sub(retained.len());
                if room == 0 {
                    *truncated = true;
                } else if count > room {
                    retained.extend_from_slice(&chunk[..room]);
                    *truncated = true;
                } else {
                    retained.extend_from_slice(&chunk[..count]);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn failure(
    kind: CommandFailureKind,
    command: &str,
    detail: &str,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    helper_reaped: bool,
) -> CommandFailure {
    CommandFailure {
        kind,
        command: command.to_owned(),
        detail: detail.to_owned(),
        stdout,
        stderr,
        helper_reaped,
    }
}

fn command_label(command: &Command) -> String {
    let mut label = command.get_program().to_string_lossy().into_owned();
    for arg in command.get_args() {
        label.push(' ');
        label.push_str(&arg.to_string_lossy());
    }
    label
}

#[cfg(test)]
#[path = "../tests/unit/lifecycle.rs"]
mod tests;
