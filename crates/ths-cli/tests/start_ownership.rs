#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

const PREFLIGHT: Duration = Duration::from_secs(6);

struct Fixture {
    bin: TempDir,
    home: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let bin = tempfile::tempdir().unwrap();
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  version) sleep {}; printf '%s\\n' '27.0.0' ;;\n  image) printf '%s\\n' 'sha256:0000000000000000000000000000000000000000000000000000000000000000' ;;\n  *) exit 0 ;;\nesac\n",
            PREFLIGHT.as_secs()
        );
        let path = bin.path().join("docker");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            bin,
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ths"));
        command
            .args(args)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.bin.path().display()),
            )
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn spawn(&self, args: &[&str]) -> Child {
        self.command(args).spawn().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
}

fn wait_for_lock(home: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let found = walk(home).iter().any(|path| path.ends_with("pre.session"));
        if found {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("start never published its session");
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk(&path));
            } else {
                found.push(path);
            }
        }
    }
    found
}

#[test]
fn a_start_in_docker_preflight_already_owns_the_environment() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let mut first = fixture.spawn(&["--name", "pre", "start", "--no-open"]);
    wait_for_lock(fixture.home.path());

    let second = fixture.run(&["--name", "pre", "start", "--no-open"]);
    assert_eq!(second.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&second.stderr)
            .contains("already running in another `ths start` or `ths run`"),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(started.elapsed() < PREFLIGHT);
    assert!(first.try_wait().unwrap().is_none());

    let stop = fixture.run(&["--name", "pre", "stop"]);
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    let first = first.wait_with_output().unwrap();
    assert_eq!(first.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&first.stderr)
            .contains("pre was stopped by another command during startup"),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(started.elapsed() < PREFLIGHT);
}

#[test]
fn a_run_in_docker_preflight_already_owns_the_environment() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let mut run = fixture.spawn(&["--name", "pre", "run", "--", "true"]);
    wait_for_lock(fixture.home.path());

    let start = fixture.run(&["--name", "pre", "start", "--no-open"]);
    assert_eq!(start.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&start.stderr)
            .contains("already running in another `ths start` or `ths run`"),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(run.try_wait().unwrap().is_none());

    let stop = fixture.run(&["--name", "pre", "stop"]);
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(String::from_utf8_lossy(&stop.stdout).contains("Asking the launcher"));
    let run = run.wait_with_output().unwrap();
    assert_eq!(run.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&run.stderr)
            .contains("pre was stopped by another command during startup"),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(started.elapsed() < PREFLIGHT);
}

#[test]
fn wait_outlasts_a_start_in_slow_docker_preflight() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let mut start = fixture.spawn(&["--name", "pre", "start", "--no-open"]);
    let mut wait = fixture.spawn(&["--name", "pre", "wait", "--timeout", "30"]);

    loop {
        let start_done = start.try_wait().unwrap().is_some();
        if let Some(status) = wait.try_wait().unwrap() {
            let output = wait.wait_with_output().unwrap();
            assert!(
                start_done,
                "wait exited with {status} after {:?} while start was still running: {}",
                started.elapsed(),
                String::from_utf8_lossy(&output.stderr)
            );
            break;
        }
        if start_done && started.elapsed() > Duration::from_secs(40) {
            panic!("wait outlived its timeout");
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(started.elapsed() >= PREFLIGHT);
    start.wait().unwrap();
}

#[test]
fn wait_outlasts_a_run_in_slow_docker_preflight() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let mut start = fixture.spawn(&["--name", "pre", "run", "--", "true"]);
    let mut wait = fixture.spawn(&["--name", "pre", "wait", "--timeout", "30"]);

    loop {
        let start_done = start.try_wait().unwrap().is_some();
        if let Some(status) = wait.try_wait().unwrap() {
            let output = wait.wait_with_output().unwrap();
            assert!(
                start_done,
                "wait exited with {status} after {:?} while run was still running: {}",
                started.elapsed(),
                String::from_utf8_lossy(&output.stderr)
            );
            break;
        }
        if start_done && started.elapsed() > Duration::from_secs(40) {
            panic!("wait outlived its timeout");
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(started.elapsed() >= PREFLIGHT);
    start.wait().unwrap();
}
