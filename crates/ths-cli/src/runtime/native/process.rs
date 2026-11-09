use super::*;
use nix::{
    errno::Errno,
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use std::os::unix::process::CommandExt;

#[derive(Default)]
pub(super) struct Processes {
    entries: Vec<Process>,
}
struct Process {
    label: String,
    child: Child,
    monitored: bool,
    stopped: bool,
}
impl Processes {
    pub(super) fn spawn(
        &mut self,
        label: &str,
        command: &mut Command,
        state: Option<&Path>,
    ) -> Result<usize> {
        command.process_group(0);
        if let Some(state) = state {
            let log = File::create(state.join("logs").join(format!("{label}.log")))?;
            command.stdout(log.try_clone()?).stderr(log);
        }
        let child = command
            .spawn()
            .with_context(|| format!("starting {label}"))?;
        self.entries.push(Process {
            label: label.into(),
            child,
            monitored: false,
            stopped: false,
        });
        Ok(self.entries.len() - 1)
    }
    pub(super) fn pid(&self, id: usize) -> u32 {
        self.entries[id].child.id()
    }
    pub(super) fn monitor(&mut self, id: usize) {
        self.entries[id].monitored = true;
    }
    pub(super) fn status(&mut self, id: usize) -> Result<Option<ExitStatus>> {
        Ok(self.entries[id].child.try_wait()?)
    }
    pub(super) fn check(&mut self) -> Result<()> {
        for entry in &mut self.entries {
            if entry.monitored
                && !entry.stopped
                && let Some(status) = entry.child.try_wait()?
            {
                bail!(
                    "{} exited with {status}; inspect its native logs",
                    entry.label
                );
            }
        }
        Ok(())
    }
    pub(super) fn stop_one(&mut self, id: usize) -> Result<()> {
        stop(
            &mut self.entries[id],
            Instant::now() + Duration::from_secs(3),
        )
    }
    pub(super) fn stop_all(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut errors = Vec::new();
        // Notify all groups before waiting, so cleanup has one shared budget.
        for entry in self.entries.iter_mut().rev().filter(|entry| !entry.stopped) {
            if let Err(error) = entry
                .child
                .try_wait()
                .map_err(anyhow::Error::from)
                .and_then(|_| signal(entry, Signal::SIGTERM))
            {
                errors.push(format!("{}: {error:#}", entry.label));
            }
        }
        for entry in self.entries.iter_mut().rev() {
            if let Err(error) = stop(entry, deadline) {
                errors.push(format!("{}: {error:#}", entry.label));
            }
        }
        if !errors.is_empty() {
            bail!("native cleanup failed: {}", errors.join("; "));
        }
        Ok(())
    }
}
impl Drop for Processes {
    fn drop(&mut self) {
        if self.entries.iter().any(|entry| !entry.stopped)
            && let Err(error) = self.stop_all()
        {
            eprintln!("{error:#}");
        }
    }
}
fn signal(entry: &Process, signal: Signal) -> Result<()> {
    match killpg(Pid::from_raw(entry.child.id() as i32), signal) {
        // macOS can report EPERM while the group leader becomes a zombie;
        // stop still requires the whole group to disappear before marking it clean.
        Ok(()) | Err(Errno::ESRCH) | Err(Errno::EPERM) => Ok(()),
        Err(error) => Err(error.into()),
    }
}
fn stop(entry: &mut Process, deadline: Instant) -> Result<()> {
    if entry.stopped {
        return Ok(());
    }
    entry.child.try_wait()?; // Reap the leader first: macOS rejects signaling a zombie leader.
    signal(entry, Signal::SIGTERM)?;
    loop {
        entry.child.try_wait()?;
        match killpg(Pid::from_raw(entry.child.id() as i32), None) {
            Err(Errno::ESRCH) => {
                entry.stopped = true;
                return Ok(());
            }
            Ok(()) | Err(Errno::EPERM) => {}
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    signal(entry, Signal::SIGKILL)?;
    let reap = Instant::now() + Duration::from_secs(1);
    loop {
        let reaped = entry.child.try_wait()?.is_some();
        if reaped
            && matches!(
                killpg(Pid::from_raw(entry.child.id() as i32), None),
                Err(Errno::ESRCH)
            )
        {
            entry.stopped = true;
            return Ok(());
        }
        if Instant::now() >= reap {
            bail!(
                "process group {} remains unverified after SIGKILL",
                entry.child.id()
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub(super) fn prerequisites() -> Result<()> {
    #[cfg(target_os = "macos")]
    if !Path::new("/usr/sbin/lsof").is_file() {
        bail!("native runtime requires the macOS system /usr/sbin/lsof");
    }
    #[cfg(target_os = "linux")]
    if !Path::new("/proc/self/fd").is_dir() {
        bail!("native runtime requires mounted procfs for listener ownership");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn owns_listener(pid: u32, port: u16, deadline: Deadline) -> Result<bool> {
    let mut command = Command::new("/usr/sbin/lsof");
    command.env_clear().args([
        "-nP",
        "-a",
        "-p",
        &pid.to_string(),
        &format!("-iTCP:{port}"),
        "-sTCP:LISTEN",
        "-Fpn",
    ]);
    let mut helpers = HelperSet::new();
    let output = run(
        &mut command,
        deadline,
        Cancellation::Ignore,
        OutputMode::Capture,
        &LifecyclePolicy::default(),
        &mut helpers,
    );
    match output {
        Ok(output) => {
            let text = String::from_utf8(output.stdout)?;
            if !output.stderr.is_empty() {
                bail!(
                    "listener inspection: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(text.lines().any(|line| line == format!("p{pid}"))
                && text
                    .lines()
                    .any(|line| line == format!("n127.0.0.1:{port}")))
        }
        Err(error)
            if error.kind == CommandFailureKind::Nonzero
                && error.detail.contains("exit status: 1")
                && error.stderr.is_empty() =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "linux")]
pub(super) fn owns_listener(pid: u32, port: u16, deadline: Deadline) -> Result<bool> {
    let mut inodes = std::collections::HashSet::new();
    let entries = match fs::read_dir(format!("/proc/{pid}/fd")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        if deadline.expired() {
            bail!("listener inspection timed out");
        }
        match fs::read_link(entry?.path()) {
            Ok(link) => {
                if let Some(inode) = link
                    .to_string_lossy()
                    .strip_prefix("socket:[")
                    .and_then(|link| link.strip_suffix(']'))
                {
                    inodes.insert(inode.to_owned());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let sockets = fs::read_to_string(format!("/proc/{pid}/net/tcp"))?;
    let address = format!("0100007F:{port:04X}");
    Ok(sockets.lines().skip(1).any(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        fields.len() > 9 && fields[1] == address && fields[3] == "0A" && inodes.contains(fields[9])
    }))
}
