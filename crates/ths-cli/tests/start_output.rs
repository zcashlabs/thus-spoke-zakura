#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    directory: tempfile::TempDir,
    name: String,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
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
        }
    }

    fn start(&self, json: bool, fail_init: bool) -> Child {
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
                "20000",
            ])
            .env("THS_TEST_FAIL_INIT", if fail_init { "1" } else { "0" })
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
            .env("THS_TEST_PORT", "38232")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
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

fn output_after_shutdown(json: bool, fail_init: bool) -> (bool, String, String, String) {
    let fixture = Fixture::new();
    let mut child = fixture.start(json, fail_init);
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
        if ready_receiver
            .recv_timeout(Duration::from_secs(20))
            .is_err()
        {
            let _ = child.kill();
            child.wait().unwrap();
            panic!(
                "launcher did not become ready: {}",
                stderr_thread.join().unwrap()
            );
        }
        assert!(
            Command::new("/bin/kill")
                .args(["-INT", &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
    }
    let success = finish(&mut child).success();
    for kind in ["container", "volume", "network"] {
        let resources = fixture.directory.path().join("state").join(kind);
        assert!(
            fs::read_dir(resources).unwrap().next().is_none(),
            "fake {kind} resources were not removed"
        );
    }
    (
        success,
        stdout_thread.join().unwrap(),
        stderr_thread.join().unwrap(),
        fs::read_to_string(fixture.directory.path().join("commands")).unwrap(),
    )
}

#[test]
fn json_start_stdout_is_one_document_after_interrupt_and_cleanup() {
    let (success, stdout, stderr, commands) = output_after_shutdown(true, false);
    assert!(success, "{stderr}");
    let endpoints: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(endpoints["network"], "regtest");
    assert_eq!(endpoints["dashboard"], "http://127.0.0.1:52805");
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
    let (success, stdout, stderr, commands) = output_after_shutdown(true, true);
    assert!(!success);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("disposable test mnemonic"));
    assert!(stderr.contains("failed"));
    assert!(commands.contains("rm -f"));
    assert!(commands.contains("volume rm"));
    assert!(commands.contains("network rm"));
}

#[test]
fn human_start_keeps_lifecycle_and_docker_output_on_stdout() {
    let (success, stdout, stderr, _) = output_after_shutdown(false, false);
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
    assert!(stderr.contains("init diagnostic"));
    assert!(!stderr.contains("Preparing a fresh"));
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
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains(diagnostic)
        );
    }
}
