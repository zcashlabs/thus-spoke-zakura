use super::*;
use crate::client::{activity_history, render_activity_history};
use std::io::{IsTerminal, Write};

const WATCH_INTERVAL: Duration = Duration::from_secs(2);
// Bounds how long a stalled read can delay Ctrl+C and deletion handling.
const WATCH_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";

pub(super) enum Tick {
    Poll,
    Ended(Ended),
}

pub(super) enum EnvironmentState {
    Running,
    Stopped,
    Deleted,
}

enum WatchState {
    Fresh,
    Stopped,
    Unreachable(String),
}

impl Runtime {
    /// Follows the environment's recent-activity window until interrupted.
    /// Reads only; it never submits or retries payments.
    pub fn wallet_activity_watch(
        &self,
        name: &InstanceName,
        limit: Option<u32>,
        json: bool,
    ) -> Result<()> {
        let shutdown = Shutdown::install()?;
        let (dashboard, client) = self.instance_client_with_timeout(name, WATCH_REQUEST_TIMEOUT)?;
        let app_container = format!("{}-app", prefix(name));
        // Watching the app container ends the loop promptly on `stop`/`reset`
        // from another shell. When it cannot be resolved, the record check
        // still detects deletion on the next failed read.
        let _monitor = watched_app_container(&app_container).ok().map(|app| {
            DeletionMonitor::spawn(
                Arc::new(DockerEvents::default()),
                &app,
                DELETION_WATCH_RETRY,
                &shutdown,
            )
        });
        self.wallet_activity_watch_with(
            name,
            &dashboard,
            &client,
            std::io::stdout().lock(),
            std::io::stderr().lock(),
            std::io::stdout().is_terminal(),
            limit,
            json,
            &|| self.environment_state(name, &app_container),
            &|duration| poll_tick(&shutdown, duration),
        )
    }

    /// Distinguishes a stopped environment from one that is restarting or
    /// deleted, after a failed activity read.
    fn environment_state(&self, name: &InstanceName, app_container: &str) -> EnvironmentState {
        if self.read_instance(name).is_err()
            && !self.instance_dir(name).join("instance.json").exists()
        {
            return EnvironmentState::Deleted;
        }
        match container_running(app_container) {
            Ok(false) => EnvironmentState::Stopped,
            _ => EnvironmentState::Running,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn wallet_activity_watch_with(
        &self,
        name: &InstanceName,
        dashboard: &str,
        client: &reqwest::blocking::Client,
        mut out: impl Write,
        mut err_out: impl Write,
        tty: bool,
        limit: Option<u32>,
        json: bool,
        probe: &dyn Fn() -> EnvironmentState,
        tick: &dyn Fn(Duration) -> Result<Tick>,
    ) -> Result<()> {
        let mut last_body: Option<String> = None;
        let mut last_update: Option<String> = None;
        let mut emitted: Option<String> = None;
        let mut warned: Option<String> = None;

        loop {
            let state = match activity_history(dashboard, client, name, limit) {
                Ok(activity) => {
                    last_update = Some(watch_timestamp());
                    warned = None;
                    last_body = Some(if json {
                        serde_json::to_string(&activity)?
                    } else {
                        render_activity_history(&activity, false)?
                    });
                    WatchState::Fresh
                }
                Err(error) => match probe() {
                    EnvironmentState::Deleted => bail!("environment {name} was deleted"),
                    EnvironmentState::Stopped => WatchState::Stopped,
                    EnvironmentState::Running => WatchState::Unreachable(format!("{error:#}")),
                },
            };

            if json {
                // stdout carries one snapshot line per change; status goes to
                // stderr so the stream stays parseable.
                if let Some(body) = &last_body
                    && emitted.as_deref() != Some(body)
                {
                    writeln!(out, "{body}")?;
                    emitted = Some(body.clone());
                }
                let marker = state_marker(&state);
                if !marker.is_empty() && warned.as_deref() != Some(marker) {
                    writeln!(
                        err_out,
                        "{}",
                        status_line(name, last_update.as_deref(), &state)
                    )?;
                    warned = Some(marker.to_owned());
                }
            } else {
                let header = status_line(name, last_update.as_deref(), &state);
                let body = last_body.as_deref().unwrap_or(match &state {
                    WatchState::Fresh => "No wallet activity yet.",
                    _ => "(waiting for the first snapshot)",
                });
                if tty {
                    out.write_all(CLEAR_SCREEN.as_bytes())?;
                    writeln!(out, "{header}\n\n{body}")?;
                    out.flush()?;
                } else {
                    // Dedupe on the state and snapshot; the header's timestamp
                    // is display-only.
                    let key = format!("{}\n{body}", state_marker(&state));
                    if emitted.as_deref() != Some(&key) {
                        writeln!(out, "{header}\n\n{body}\n")?;
                        emitted = Some(key);
                    }
                }
            }

            match tick(WATCH_INTERVAL)? {
                Tick::Poll => {}
                Tick::Ended(Ended::Interrupted) => return Ok(()),
                Tick::Ended(Ended::DeletedElsewhere) => bail!("environment {name} was deleted"),
            }
        }
    }
}

fn watched_app_container(app_container: &str) -> Result<WatchedContainer> {
    let details = docker_output_args(&[
        "container",
        "inspect",
        "--format",
        "{{.Id}} {{.Created}}",
        app_container,
    ])?;
    let (id, created) = details
        .split_once(' ')
        .context("reading the app container's identity")?;
    Ok(WatchedContainer {
        id: id.to_owned(),
        created: created.trim_end().to_owned(),
    })
}

fn poll_tick(shutdown: &Shutdown, duration: Duration) -> Result<Tick> {
    if shutdown.flag.load(Ordering::SeqCst) {
        return Ok(Tick::Ended(Ended::Interrupted));
    }
    match shutdown.receiver.recv_timeout(duration) {
        Ok(ended) => Ok(Tick::Ended(ended)),
        Err(RecvTimeoutError::Timeout) => Ok(Tick::Poll),
        Err(RecvTimeoutError::Disconnected) if shutdown.flag.load(Ordering::SeqCst) => {
            Ok(Tick::Ended(Ended::Interrupted))
        }
        Err(RecvTimeoutError::Disconnected) => Err(anyhow!("waiting for a shutdown signal")),
    }
}

fn state_marker(state: &WatchState) -> &str {
    match state {
        WatchState::Fresh => "",
        WatchState::Stopped => "stopped",
        WatchState::Unreachable(error) => error.as_str(),
    }
}

fn status_line(name: &InstanceName, last_update: Option<&str>, state: &WatchState) -> String {
    let at = last_update.unwrap_or("never");
    match state {
        WatchState::Fresh => format!("Watching {name} · last update {at}"),
        WatchState::Stopped => {
            format!("Watching {name} · last update {at} · environment {name} stopped")
        }
        WatchState::Unreachable(error) => {
            format!("Watching {name} · last update {at} · unreachable ({error})")
        }
    }
}

fn watch_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        % 86_400;
    format!(
        "{:02}:{:02}:{:02} UTC",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}
