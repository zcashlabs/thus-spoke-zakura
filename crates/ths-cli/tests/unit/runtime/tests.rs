use super::*;
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

#[path = "tests/client.rs"]
mod client;
#[path = "tests/images.rs"]
mod images;
#[path = "tests/instance.rs"]
mod instance;
#[path = "tests/intents.rs"]
mod intents;
#[path = "tests/logs.rs"]
mod logs;
#[path = "tests/resources.rs"]
mod resources;
#[path = "tests/startup.rs"]
mod startup;

struct RecordedResource {
    name: String,
    body: String,
}

struct RecordingDocker {
    extra_output: BTreeMap<String, String>,
    containers: Mutex<Vec<RecordedResource>>,
    volumes: Mutex<Vec<RecordedResource>>,
    networks: Mutex<Vec<RecordedResource>>,
    runs: Mutex<Vec<String>>,
    retain: Mutex<Vec<String>>,
}

fn recorded_names(text: &str) -> Vec<RecordedResource> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|name| RecordedResource {
            name: name.to_owned(),
            body: String::new(),
        })
        .collect()
}

impl RecordingDocker {
    fn new(containers: &str, volumes: &str, networks: &str) -> Self {
        Self {
            extra_output: BTreeMap::new(),
            containers: Mutex::new(recorded_names(containers)),
            volumes: Mutex::new(recorded_names(volumes)),
            networks: Mutex::new(recorded_names(networks)),
            runs: Mutex::new(Vec::new()),
            retain: Mutex::new(Vec::new()),
        }
    }

    fn inspect(&mut self, kind: &str, target: &str, body: &str) {
        let resources = match kind {
            "container" => &self.containers,
            "volume" => &self.volumes,
            _ => &self.networks,
        };
        let mut resources = resources.lock().unwrap();
        if let Some(resource) = resources
            .iter_mut()
            .find(|resource| resource.name == target)
        {
            resource.body = body.to_owned();
        } else {
            resources.push(RecordedResource {
                name: target.to_owned(),
                body: body.to_owned(),
            });
        }
    }
}

fn resource_matches(resource: &RecordedResource, token: &str) -> bool {
    resource.name == token
        || resource.body.contains(&format!("\"Id\":\"{token}\""))
        || resource.body.contains(&format!("\"Name\":\"{token}\""))
}

impl DockerResourceCommands for RecordingDocker {
    fn output(&self, args: &[&str]) -> Result<String> {
        if let Some(output) = self.extra_output.get(&args.join(" ")) {
            return Ok(output.clone());
        }
        let listed = |resources: &Mutex<Vec<RecordedResource>>| {
            resources
                .lock()
                .unwrap()
                .iter()
                .map(|resource| resource.name.clone())
                .collect::<Vec<_>>()
                .join("\n")
        };
        match args {
            ["container", "ls", "-a", "--format", "{{.Names}}"] => Ok(listed(&self.containers)),
            ["volume", "ls", "--format", "{{.Name}}"] => Ok(listed(&self.volumes)),
            ["network", "ls", "--format", "{{.Name}}"] => Ok(listed(&self.networks)),
            [kind, "inspect", target] => {
                let resources = match *kind {
                    "container" => &self.containers,
                    "volume" => &self.volumes,
                    "network" => &self.networks,
                    _ => bail!("unexpected Docker read: {}", args.join(" ")),
                };
                resources
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|resource| resource.name == *target || resource_matches(resource, target))
                    .map(|resource| resource.body.clone())
                    .filter(|body| !body.is_empty())
                    .ok_or_else(|| anyhow!("unexpected Docker read: {}", args.join(" ")))
            }
            _ => bail!("unexpected Docker read: {}", args.join(" ")),
        }
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        self.runs.lock().unwrap().push(args.join(" "));
        let token = args.last().copied().unwrap_or("");
        if self.retain.lock().unwrap().iter().any(|kept| kept == token) {
            return Ok(());
        }
        let resources = match args {
            ["rm", "-f", _] => &self.containers,
            ["volume", "rm", _] => &self.volumes,
            ["network", "rm", _] => &self.networks,
            _ => return Ok(()),
        };
        resources
            .lock()
            .unwrap()
            .retain(|resource| !resource_matches(resource, token));
        Ok(())
    }
}

struct RecordingHost {
    events: Arc<Mutex<Vec<String>>>,
    initial_delete_error: bool,
    shutdown_collision: Option<RecordingDocker>,
    wait_ready_result: Result<(), String>,
    open_url_result: Result<(), String>,
    interrupt_before_ready: bool,
    final_cleanup_failure: Option<String>,
    deleted_elsewhere: bool,
    allocate_delay: Duration,
    ready_delay: Duration,
    run_status: Option<RuntimeStatus>,
    while_running: Option<Box<dyn Fn() -> Ended>>,
    real_child: bool,
    stopping_seen: Arc<AtomicBool>,
    ready_in_teardown: Arc<AtomicBool>,
}

impl RecordingHost {
    fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events: events.clone(),
                initial_delete_error: false,
                shutdown_collision: None,
                wait_ready_result: Ok(()),
                open_url_result: Ok(()),
                interrupt_before_ready: false,
                final_cleanup_failure: None,
                deleted_elsewhere: false,
                allocate_delay: Duration::ZERO,
                ready_delay: Duration::ZERO,
                run_status: None,
                while_running: None,
                real_child: false,
                stopping_seen: Arc::new(AtomicBool::new(false)),
                ready_in_teardown: Arc::new(AtomicBool::new(false)),
            },
            events,
        )
    }

    fn note_stopping(&self, runtime: &Runtime, name: &InstanceName) {
        if runtime
            .root
            .join(".locks")
            .join(format!("{name}.stopping"))
            .exists()
        {
            self.stopping_seen.store(true, Ordering::SeqCst);
        }
        if let Ok(instance) = runtime.read_instance(name)
            && runtime.ready_owner_wrote(name, &instance).unwrap()
        {
            self.ready_in_teardown.store(true, Ordering::SeqCst);
        }
    }

    fn push(&self, event: &str) {
        self.events.lock().unwrap().push(event.to_owned());
    }
}

fn verified_report() -> CleanupReport {
    CleanupReport {
        outcome: CleanupOutcome::VerifiedComplete,
        failures: Vec::new(),
        recovery_path: None,
        helpers_finished: true,
    }
}

impl StartHost for RecordingHost {
    fn runtime_status(&self, _endpoints: &Endpoints, _timeout: Duration) -> Result<RuntimeStatus> {
        self.push("runtime_status");
        self.run_status
            .clone()
            .context("no recorded runtime status")
    }

    fn run_child(
        &self,
        command: &[OsString],
        current_dir: &Path,
        name: &InstanceName,
        endpoints: &Endpoints,
        shutdown: &Shutdown,
    ) -> Result<ChildOutcome> {
        self.push(&format!("child:{name}"));
        if self.real_child {
            return run_child(
                command,
                current_dir,
                name,
                endpoints,
                shutdown,
                Duration::from_millis(100),
            );
        }
        Ok(ChildOutcome::Exited(42))
    }

    fn delete(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        context: &LifecycleContext,
        _deadline: Deadline,
    ) -> Result<CleanupReport> {
        let later = self
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.starts_with("delete:") || event.starts_with("delete_partial:"));
        self.push(&format!("delete:{name}"));
        if later {
            self.push(&format!("final_cleanup:{name}"));
            self.note_stopping(runtime, name);
        }
        if !later && self.initial_delete_error {
            bail!("resource collision");
        }
        if later
            && self
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.starts_with("wait_for_shutdown"))
            && let Some(docker) = &self.shutdown_collision
        {
            return runtime.cleanup_with(name, docker, false, &context.policy);
        }
        if (later || self.run_status.is_some())
            && let Some(message) = &self.final_cleanup_failure
        {
            return Ok(CleanupReport {
                outcome: CleanupOutcome::Uncertain,
                failures: vec![message.clone()],
                recovery_path: Some(runtime.recovery_path(name)),
                helpers_finished: true,
            });
        }
        Ok(verified_report())
    }

    fn delete_partial(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        context: &LifecycleContext,
        deadline: Deadline,
    ) -> Result<CleanupReport> {
        self.note_stopping(runtime, name);
        if let Some(docker) = &self.shutdown_collision {
            let later =
                self.events.lock().unwrap().iter().any(|event| {
                    event.starts_with("delete:") || event.starts_with("delete_partial:")
                });
            self.push(&format!("delete_partial:{name}"));
            if later {
                self.push(&format!("final_cleanup:{name}"));
            }
            runtime.cleanup_with(name, docker, true, &context.policy)
        } else {
            self.delete(runtime, name, context, deadline)
        }
    }

    fn allocate(
        &self,
        _runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
        _port_offset: u16,
        _context: &LifecycleContext,
        _journal: &mut recovery::RecoveryJournal,
    ) -> Result<AllocatedInstance> {
        self.push(&format!("allocate:{name}"));
        std::thread::sleep(self.allocate_delay);
        shutdown.check()?;
        Ok(AllocatedInstance {
            endpoints: Endpoints {
                dashboard: "http://127.0.0.1:1".into(),
                rpc: "http://127.0.0.1:2".into(),
                lightwalletd: "http://127.0.0.1:3".into(),
                p2p: "127.0.0.1:4".into(),
                network: default_regtest(),
                tls: false,
            },
            resources: Vec::new(),
            app_container: watched("app-1"),
            zakura_container_id: "zakura-id".into(),
        })
    }

    fn wait_ready(
        &self,
        _allocated: &AllocatedInstance,
        shutdown: &Shutdown,
        _context: &LifecycleContext,
    ) -> Result<()> {
        self.push("wait_ready");
        std::thread::sleep(self.ready_delay);
        if self.interrupt_before_ready {
            bail!("interrupted");
        }
        shutdown.check()?;
        self.wait_ready_result
            .as_ref()
            .map(|_| ())
            .map_err(|e| anyhow!("{e}"))
    }

    fn open_url(&self, url: &str, _shutdown: &Shutdown, _context: &LifecycleContext) -> Result<()> {
        self.push(&format!("open_url:{url}"));
        self.open_url_result
            .as_ref()
            .map(|_| ())
            .map_err(|e| anyhow!("{e}"))
    }

    fn wait_for_shutdown(
        &self,
        app_container: &WatchedContainer,
        shutdown: &Shutdown,
    ) -> Result<Ended> {
        self.push(&format!("wait_for_shutdown:{}", app_container.id));
        assert_eq!(app_container, &watched("app-1"));
        if let Some(while_running) = &self.while_running {
            return Ok(while_running());
        }
        if self.shutdown_collision.is_some() {
            return Ok(Ended::Interrupted);
        }
        let events = ScriptedEvents::default();
        if self.deleted_elsewhere {
            events.streams.lock().unwrap().push_back(Ok(true));
        }
        let _monitor = DeletionMonitor::spawn(
            Arc::new(events),
            app_container,
            Duration::from_millis(1),
            shutdown,
        );
        shutdown.wait()
    }
}

fn runtime_for_tests() -> Runtime {
    let root = std::env::temp_dir().join(format!("ths-start-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    Runtime { root }
}

fn name(value: &str) -> InstanceName {
    value.parse().unwrap()
}

#[test]
fn overlapping_faucet_process_rejects_the_locked_journal() {
    intents::overlapping_faucet_process_rejects_the_locked_journal();
}

#[test]
fn faucet_journal_lock_probe() {
    let Some(dir) = std::env::var_os("THS_FAUCET_JOURNAL_LOCK_PROBE") else {
        return;
    };
    let error = IntentJournal::open(std::path::Path::new(&dir), "faucet")
        .err()
        .expect("overlapping command acquired the lock");
    assert!(
        error
            .to_string()
            .contains("another faucet command is running")
    );
}

/// answers loopback requests in turn with canned statuses and bodies, passing on each
/// request line and body. `drop` closes the connection without a response, as a lost
/// response would; `hang` holds it open without answering; `delay` waits 200 ms before a 200.
fn serve<B: Into<String> + Send + 'static>(
    responses: Vec<(&'static str, B)>,
) -> (String, mpsc::Receiver<(String, String)>) {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (requests, received) = mpsc::channel();
    std::thread::spawn(move || {
        for (status, body) in responses {
            let body = body.into();
            let (mut stream, request_line, request) = loop {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                if request_line.starts_with("HEAD ") {
                    // Local health probes can connect to ephemeral test ports.
                    continue;
                }
                let mut request = vec![0; length];
                reader.read_exact(&mut request).unwrap();
                break (stream, request_line, request);
            };
            requests
                .send((
                    request_line.trim_end().to_owned(),
                    String::from_utf8(request).unwrap(),
                ))
                .unwrap();
            let (status, tolerate_write_error) = match status {
                "drop" => continue,
                "hang" => {
                    std::thread::sleep(Duration::from_secs(60));
                    continue;
                }
                "delay" => {
                    std::thread::sleep(Duration::from_millis(200));
                    ("200 OK", true)
                }
                status => (status, false),
            };
            let response = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            if !tolerate_write_error {
                response.unwrap();
            }
        }
    });
    (url, received)
}
struct IdleDocker;

impl DockerResourceCommands for IdleDocker {
    fn output(&self, args: &[&str]) -> Result<String> {
        bail!("unexpected Docker read: {args:?}")
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        bail!("unexpected Docker run: {args:?}")
    }
}

#[path = "tests/deletion.rs"]
mod deletion;
use deletion::{ScriptedEvents, watched};

thread_local! {
    /// Points this test thread's Docker commands at a daemon that does not exist.
    pub(super) static DOCKER_UNREACHABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn with_unreachable_docker<T>(test: impl FnOnce() -> T) -> T {
    DOCKER_UNREACHABLE.set(true);
    let result = test();
    DOCKER_UNREACHABLE.set(false);
    result
}

#[path = "tests/runner.rs"]
mod runner;

#[test]
fn send_interruption_probe() {
    client::send_interruption_probe();
}
