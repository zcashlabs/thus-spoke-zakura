#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    sync::Arc,
    sync::{
        atomic::{AtomicU16, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

static NEXT_PORT_OFFSET: AtomicU16 = AtomicU16::new(20_000);

fn next_port_offset(offsets: &AtomicU16) -> u16 {
    loop {
        let offset = offsets.fetch_add(10, Ordering::Relaxed);
        assert!(offset <= 32_730, "no free port set for the startup fixture");
        // Startup checks all four ports before the fake readiness servers bind.
        let listeners: std::io::Result<Vec<_>> = [32_805, 18_232, 18_233, 9_067]
            .into_iter()
            .map(|port| TcpListener::bind(("127.0.0.1", port + offset)))
            .collect();
        match listeners {
            Ok(_) => return offset,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("checking startup fixture ports at offset {offset}: {error}"),
        }
    }
}

#[test]
fn fixture_skips_each_occupied_published_port() {
    let offsets = AtomicU16::new(30_000);
    for port in [32_805, 18_232, 18_233, 9_067] {
        let (offset, listener) = loop {
            let offset = next_port_offset(&offsets);
            if let Ok(listener) = TcpListener::bind(("127.0.0.1", port + offset)) {
                break (offset, listener);
            }
        };
        offsets.store(offset, Ordering::Relaxed);
        let selected = next_port_offset(&offsets);
        assert_ne!(
            selected,
            offset,
            "fixture selected occupied port {}",
            listener.local_addr().unwrap()
        );
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    name: String,
    port_offset: u16,
    stop_server: Arc<std::sync::atomic::AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let port_offset = next_port_offset(&NEXT_PORT_OFFSET);
        let commands_path = directory.path().join("commands");
        let stop_server = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = Arc::clone(&stop_server);
        let server = thread::spawn(move || {
            while !server_stop.load(Ordering::Relaxed)
                && !fs::read_to_string(&commands_path)
                    .is_ok_and(|commands| commands.contains("start -a"))
            {
                thread::sleep(Duration::from_millis(5));
            }
            if server_stop.load(Ordering::Relaxed) {
                return;
            }
            let listeners = [
                TcpListener::bind(("127.0.0.1", 18_232 + port_offset)).unwrap(),
                TcpListener::bind(("127.0.0.1", 32_805 + port_offset)).unwrap(),
            ];
            for listener in &listeners {
                listener.set_nonblocking(true).unwrap();
            }
            while !server_stop.load(Ordering::Relaxed) {
                let mut accepted = false;
                for listener in &listeners {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            accepted = true;
                            respond_ready(stream);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(error) => panic!("readiness fixture server failed: {error}"),
                    }
                }
                if !accepted {
                    thread::sleep(Duration::from_millis(5));
                }
            }
        });
        for (name, script) in [
            ("docker", include_str!("fixtures/docker.sh")),
            ("curl", include_str!("fixtures/curl.sh")),
        ] {
            let path = directory.path().join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self {
            directory,
            name: format!("output-{}", uuid::Uuid::new_v4().simple()),
            port_offset,
            stop_server,
            server: Some(server),
        }
    }

    fn start(&self, json: bool, fail_init: bool, deleted_elsewhere: bool) -> Child {
        let mut command = self.command();
        if json {
            command.arg("--json");
        }
        // Each subprocess gets its own configuration and fake executables.
        // No real Docker resources are touched by these lifecycle tests.
        command
            .args([
                "--name",
                &self.name,
                "start",
                "--no-open",
                "--port-offset",
                &self.port_offset.to_string(),
            ])
            .env("THS_TEST_FAIL_INIT", if fail_init { "1" } else { "0" })
            .env(
                "THS_TEST_DELETE_ELSEWHERE",
                if deleted_elsewhere { "1" } else { "0" },
            )
            .spawn()
            .unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ths"));
        command
            .env("HOME", self.directory.path())
            .env("XDG_CONFIG_HOME", self.directory.path())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.directory.path().display()),
            )
            .env("THS_TEST_STATE", self.directory.path().join("state"))
            .env("THS_TEST_COMMANDS", self.directory.path().join("commands"))
            .env("THS_TEST_NAME", &self.name)
            .env("THS_TEST_PORT", (18_232 + self.port_offset).to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn start_with_browser_failure(&self) -> Child {
        self.start_with_browser_opener(
            "#!/bin/sh\nprintf '%s\\n' called > \"$THS_TEST_OPEN_MARKER\"\nexit 1\n",
        )
    }

    fn start_with_browser_opener(&self, script: &str) -> Child {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let opener_path = self.directory.path().join(opener);
        fs::write(&opener_path, script).unwrap();
        fs::set_permissions(opener_path, fs::Permissions::from_mode(0o755)).unwrap();

        self.command()
            .args([
                "--json",
                "--name",
                &self.name,
                "start",
                "--port-offset",
                &self.port_offset.to_string(),
            ])
            .env("THS_TEST_FAIL_INIT", "0")
            .env("THS_TEST_DELETE_ELSEWHERE", "0")
            .env(
                "THS_TEST_OPEN_MARKER",
                self.directory.path().join("open-marker"),
            )
            .spawn()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop_server.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}

fn respond_ready(mut stream: TcpStream) {
    let mut request = [0; 2048];
    let _ = stream.read(&mut request);
    let body = r#"{"result":"test-tip"}"#;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
}

fn finish(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("launcher did not exit within the fixture deadline");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn output_after_shutdown(
    json: bool,
    fail_init: bool,
    deleted_elsewhere: bool,
) -> (bool, String, String, String, u16) {
    let fixture = Fixture::new();
    let mut child = fixture.start(json, fail_init, deleted_elsewhere);
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (ready_sender, ready_receiver) = mpsc::channel();
    let stdout_thread = thread::spawn(move || {
        let mut output = String::new();
        for line in BufReader::new(stdout).lines() {
            output.push_str(&line.unwrap());
            output.push('\n');
            if serde_json::from_str::<serde_json::Value>(&output).is_ok()
                || (!json && output.contains("is ready"))
            {
                let _ = ready_sender.send(());
            }
        }
        output
    });
    let stderr_thread = thread::spawn(move || std::io::read_to_string(stderr).unwrap());
    if !fail_init {
        let saw_ready = ready_receiver.recv_timeout(Duration::from_secs(20)).is_ok();
        if !saw_ready {
            let _ = child.kill();
            child.wait().unwrap();
            panic!(
                "launcher did not become ready; stderr: {}",
                stderr_thread.join().unwrap(),
            );
        }
        if deleted_elsewhere {
            // Simulate another command deleting only this fixture's fake resources.
            for kind in ["container", "volume", "network"] {
                let resources = fixture.directory.path().join("state").join(kind);
                for resource in fs::read_dir(resources).unwrap() {
                    fs::remove_file(resource.unwrap().path()).unwrap();
                }
            }
            fs::write(fixture.directory.path().join("state/deleted"), "").unwrap();
        } else {
            assert!(
                Command::new("/bin/kill")
                    .args(["-INT", &child.id().to_string()])
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }
    let success = finish(&mut child).success();
    let stdout = stdout_thread.join().unwrap();
    let stderr = stderr_thread.join().unwrap();
    for kind in ["container", "volume", "network"] {
        let resources = fixture.directory.path().join("state").join(kind);
        assert!(
            fs::read_dir(&resources)
                .unwrap_or_else(|error| panic!(
                    "reading {}: {error}; launcher stderr: {stderr}",
                    resources.display()
                ))
                .next()
                .is_none(),
            "fake {kind} resources were not removed; launcher stderr: {stderr}"
        );
    }
    (
        success,
        stdout,
        stderr,
        fs::read_to_string(fixture.directory.path().join("commands")).unwrap(),
        fixture.port_offset,
    )
}

#[test]
fn json_start_stdout_is_one_document_after_interrupt_and_cleanup() {
    let (success, stdout, stderr, commands, port_offset) =
        output_after_shutdown(true, false, false);
    assert!(success, "{stderr}");
    let endpoints: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(endpoints["network"], "regtest");
    assert_eq!(
        endpoints["dashboard"],
        format!("http://127.0.0.1:{}", 32_805 + port_offset)
    );
    for diagnostic in [
        "Docker test-docker",
        "Preparing a fresh",
        "Starting",
        "disposable test mnemonic",
        "init diagnostic",
        "Stopping and deleting",
        "Deleted",
        "docker lifecycle output",
    ] {
        assert!(
            stderr.contains(diagnostic),
            "missing {diagnostic}: {stderr}"
        );
    }
    assert!(commands.contains("rm -f"));
    assert!(commands.contains("volume rm"));
    assert!(commands.contains("network rm"));
}

#[test]
fn json_start_failure_keeps_stdout_empty_through_partial_cleanup() {
    let (success, stdout, stderr, commands, _) = output_after_shutdown(true, true, false);
    assert!(!success);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("disposable test mnemonic"));
    assert!(stderr.contains("nonzero exit"), "{stderr}");
    assert!(commands.contains("rm -f"));
    assert!(commands.contains("volume rm"));
    assert!(commands.contains("network rm"));
}

#[test]
fn human_start_keeps_lifecycle_and_docker_output_on_stdout() {
    let (success, stdout, stderr, _, _) = output_after_shutdown(false, false, false);
    assert!(success, "{stderr}");
    for diagnostic in [
        "Docker test-docker",
        "Preparing a fresh",
        "is ready",
        "disposable test mnemonic",
        "Stopping and deleting",
        "Deleted",
        "docker lifecycle output",
    ] {
        assert!(
            stdout.contains(diagnostic),
            "missing {diagnostic}: {stdout}"
        );
    }
    assert_eq!(stdout.matches("Docker test-docker").count(), 1);
    assert!(stderr.contains("init diagnostic"));
    assert!(!stderr.contains("Preparing a fresh"));
}

#[test]
fn json_start_stdout_is_one_document_after_external_deletion() {
    let (success, stdout, stderr, commands, _) = output_after_shutdown(true, false, true);
    assert!(success, "{stderr}");
    let endpoints: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(endpoints["network"], "regtest");
    assert!(stderr.contains("was stopped and deleted by another command"));
    assert!(!stderr.contains("Stopping and deleting"));
    assert!(commands.contains("events --since"));
    assert!(!commands.contains("rm -f"));
    assert!(!commands.contains("volume rm"));
    assert!(!commands.contains("network rm"));
}

#[test]
fn json_start_browser_open_failure_keeps_stdout_parseable_through_stop() {
    let fixture = Fixture::new();
    let doctor = fixture
        .command()
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    assert!(doctor.status.success());
    let doctor: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let instance_dir =
        std::path::PathBuf::from(doctor["config_dir"].as_str().unwrap()).join(&fixture.name);
    let mut child = fixture.start_with_browser_failure();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (ready_sender, ready_receiver) = mpsc::channel();
    let stdout_thread = thread::spawn(move || {
        let mut output = String::new();
        for line in BufReader::new(stdout).lines() {
            output.push_str(&line.unwrap());
            output.push('\n');
            if serde_json::from_str::<serde_json::Value>(&output).is_ok() {
                let _ = ready_sender.send(());
            }
        }
        output
    });
    let (warning_sender, warning_receiver) = mpsc::channel();
    let stderr_thread = thread::spawn(move || {
        let mut output = String::new();
        for line in BufReader::new(stderr).lines() {
            let line = line.unwrap();
            if line.contains("could not open the dashboard automatically") {
                let _ = warning_sender.send(());
            }
            output.push_str(&line);
            output.push('\n');
        }
        output
    });

    assert!(
        ready_receiver.recv_timeout(Duration::from_secs(20)).is_ok(),
        "launcher did not print endpoints"
    );
    let marker = fixture.directory.path().join("open-marker");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(marker.exists(), "browser opener was not invoked");
    assert!(
        instance_dir.join("instance.json").exists(),
        "instance metadata was not written to the configured directory"
    );
    assert!(
        warning_receiver
            .recv_timeout(Duration::from_secs(20))
            .is_ok(),
        "launcher did not report the browser failure"
    );

    let stop_deadline = Instant::now() + Duration::from_secs(20);
    let stopped = loop {
        let stopped = fixture
            .command()
            .args(["--name", &fixture.name, "stop"])
            .output()
            .unwrap();
        if stopped.status.success()
            || !String::from_utf8_lossy(&stopped.stderr).contains("already in use")
            || Instant::now() >= stop_deadline
        {
            break stopped;
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        stopped.status.success(),
        "ths stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let status = finish(&mut child);
    let stdout = stdout_thread.join().unwrap();
    let stderr = stderr_thread.join().unwrap();
    assert!(status.success(), "launcher exited {status}: {stderr}");
    let endpoints: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let dashboard = endpoints["dashboard"].as_str().unwrap();
    assert!(
        stderr.contains("could not open the dashboard automatically"),
        "{stderr}"
    );
    assert!(stderr.contains(dashboard), "{stderr}");
    assert!(
        stderr.contains(&format!("ths --name {} open", fixture.name)),
        "{stderr}"
    );

    for kind in ["container", "volume", "network"] {
        let resources = fixture.directory.path().join("state").join(kind);
        assert!(
            fs::read_dir(resources).unwrap().next().is_none(),
            "fake {kind} resources were not removed"
        );
    }
    assert!(!instance_dir.exists(), "instance metadata was not removed");
    let commands = fs::read_to_string(fixture.directory.path().join("commands")).unwrap();
    assert!(commands.contains("rm -f"));
    assert!(commands.contains("volume rm"));
    assert!(commands.contains("network rm"));
    assert_eq!(commands.matches("rm -f").count(), 4, "{commands}");
    assert_eq!(commands.matches("volume rm").count(), 4, "{commands}");
    assert_eq!(commands.matches("network rm").count(), 1, "{commands}");
}

#[test]
fn json_start_stops_cleanly_during_browser_open() {
    let fixture = Fixture::new();
    let mut child = fixture.start_with_browser_opener(
        "#!/bin/sh\nprintf '%s\\n' called > \"$THS_TEST_OPEN_MARKER\"\nsleep 30\n",
    );
    let marker = fixture.directory.path().join("open-marker");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(marker.exists(), "browser opener was not invoked");

    let stopped = fixture
        .command()
        .args(["--name", &fixture.name, "stop"])
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "ths stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let status = finish(&mut child);
    let stdout = std::io::read_to_string(child.stdout.take().unwrap()).unwrap();
    let stderr = std::io::read_to_string(child.stderr.take().unwrap()).unwrap();
    assert!(status.success(), "launcher exited {status}: {stderr}");
    serde_json::from_str::<serde_json::Value>(&stdout).unwrap();
}

#[test]
fn standalone_doctor_retains_human_and_json_output() {
    let fixture = Fixture::new();
    let human = fixture.command().arg("doctor").output().unwrap();
    assert!(human.status.success());
    assert!(
        String::from_utf8(human.stdout)
            .unwrap()
            .contains("Docker test-docker")
    );
    assert!(human.stderr.is_empty());

    let json = fixture
        .command()
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    assert!(json.status.success());
    let result: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(result["docker"], "test-docker");
    assert_eq!(result["ok"], true);
    assert!(json.stderr.is_empty());
}

#[test]
fn standalone_build_and_pull_keep_docker_output_on_stdout() {
    let fixture = Fixture::new();
    for args in [["build", "--dev"].as_slice(), ["pull"].as_slice()] {
        let output = fixture.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("Docker test-docker"));
        assert!(stdout.contains("docker lifecycle output"));
        assert!(stdout.contains("Runtime images are ready."));
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn json_start_preflight_failures_keep_stdout_empty() {
    let fixture = Fixture::new();
    for (fault, diagnostic) in [
        ("THS_TEST_FAIL_DOCTOR", "Docker is not reachable"),
        ("THS_TEST_MISSING_IMAGE", "required image"),
    ] {
        let output = fixture
            .command()
            .args(["--json", "start", "--no-open"])
            .env(fault, "1")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains(diagnostic),
            "expected {diagnostic}: {stderr}"
        );
    }
}
