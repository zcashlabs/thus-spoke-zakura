#![cfg(unix)]
use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;
fn fixture_binary() -> &'static Path {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("ths-native-fixture-{}", std::process::id()));
        assert!(
            Command::new("rustc")
                .arg("--edition=2024")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/native_service.rs"
                ))
                .arg("-o")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        path
    })
}
struct Fixture {
    temp: TempDir,
}
impl Fixture {
    fn new(mode: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Self {
            temp: tempfile::tempdir().unwrap(),
        };
        let root = fixture.temp.path();
        fs::create_dir(root.join("bin")).unwrap();
        fs::create_dir(root.join("web")).unwrap();
        fs::write(root.join("web/index.html"), "native").unwrap();
        for service in ["zakurad", "lightwalletd", "ths-server"] {
            let file = root.join("bin").join(service);
            fs::write(
                &file,
                format!(
                    "#!/bin/sh\nexec '{}' '{}' '{}' \"$@\"\n",
                    fixture_binary().display(),
                    service,
                    mode
                ),
            )
            .unwrap();
            fs::set_permissions(file, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let target = format!(
            "{}-{}",
            std::env::consts::ARCH,
            if cfg!(target_os = "macos") {
                "apple-darwin"
            } else {
                "unknown-linux-gnu"
            }
        );
        fs::write(root.join("manifest.json"),serde_json::to_vec(&serde_json::json!({"format":1,"ths_version":env!("CARGO_PKG_VERSION"),"target":target,"zakura_commit":"c572f8faf88e58dcaefd80ec8e4ed05f2d8643b6","lightwalletd_commit":"d79cd1100575ff909d70e00d5514a4092df94934"})).unwrap()).unwrap();
        fixture
    }
    fn command(&self, name: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ths"));
        cmd.env("HOME", self.temp.path())
            .env("XDG_CONFIG_HOME", self.temp.path().join("config-home"))
            .env("HTTP_PROXY", "http://foreign.invalid:1")
            .env("ZEBRA_CONFIG", "foreign")
            .arg("--name")
            .arg(name);
        cmd
    }
    fn run(&self, name: &str, args: &[&str]) -> std::process::Output {
        self.command(name)
            .arg("run")
            .arg("--native")
            .arg("--runtime-dir")
            .arg(self.temp.path())
            .args(["--timeout", "8", "--"])
            .args(args)
            .output()
            .unwrap()
    }
    fn state(&self, name: &str) -> PathBuf {
        let output = self
            .command(name)
            .args(["--json", "doctor", "--native", "--runtime-dir"])
            .arg(self.temp.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        let doctor: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        Path::new(doctor["config_dir"].as_str().unwrap()).join(name)
    }
    fn start(&self, name: &str) -> (Child, serde_json::Value) {
        let mut child = self
            .command(name)
            .args([
                "--json",
                "start",
                "--native",
                "--no-open",
                "--timeout",
                "8",
                "--runtime-dir",
            ])
            .arg(self.temp.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let endpoint =
            serde_json::from_str(&line).unwrap_or_else(|_| panic!("startup failed: {line:?}"));
        (child, endpoint)
    }
}
fn wait(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "child did not stop");
        thread::sleep(Duration::from_millis(30));
    }
}
#[test]
fn native_run_preserves_exit_codes_and_environment() {
    let fixture = Fixture::new("normal");
    let output=fixture.run("native-success",&["/bin/sh","-c","test \"$THS_NETWORK\" = regtest && test \"$THS_LIGHTWALLETD_TLS\" = false && test -n \"$THS_API_URL\" && echo child-output"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"child-output\n");
    let output = fixture.run("native-failure", &["/bin/sh", "-c", "exit 23"]);
    assert_eq!(
        output.status.code(),
        Some(23),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn native_rejects_bad_identity_genesis_and_initializer_failure() {
    for (mode, expected) in [
        ("wrong-identity", "identity"),
        ("wrong-genesis", "genesis"),
        ("init-fail", "initializer"),
    ] {
        let fixture = Fixture::new(mode);
        let output = fixture.run("bad-service", &["/usr/bin/true"]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
#[test]
fn native_start_stop_status_and_concurrent_instances_without_docker() {
    let fixture = Fixture::new("normal");
    let (mut a, ae) = fixture.start("native-a");
    let (mut b, be) = fixture.start("native-b");
    assert_ne!(ae["rpc"], be["rpc"]);
    for name in ["native-a", "native-b"] {
        let output = fixture
            .command(name)
            .args(["--json", "status"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = fixture
            .command(name)
            .args(["--json", "endpoints"])
            .output()
            .unwrap();
        assert!(output.status.success());
    }
    let output = fixture.command("native-a").arg("stop").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(wait(&mut a).success());
    assert!(b.try_wait().unwrap().is_none());
    assert!(
        fixture
            .command("native-b")
            .args(["reset", "--force"])
            .status()
            .unwrap()
            .success()
    );
    assert!(wait(&mut b).success());
}
#[test]
fn native_browser_failure_keeps_environment_running_until_stop() {
    use std::os::unix::fs::PermissionsExt;
    for mode in ["missing", "failed"] {
        let fixture = Fixture::new("normal");
        let name = "browser-failure";
        let opener_dir = fixture.temp.path().join("opener");
        fs::create_dir(&opener_dir).unwrap();
        if mode == "failed" {
            let opener = opener_dir.join(if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            });
            fs::write(&opener, "#!/bin/sh\necho opener-output\nexit 7\n").unwrap();
            fs::set_permissions(opener, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let stderr_path = fixture.temp.path().join("launcher-stderr");
        let mut child = fixture
            .command(name)
            .env("PATH", &opener_dir)
            .args([
                "--json",
                "start",
                "--native",
                "--timeout",
                "8",
                "--runtime-dir",
            ])
            .arg(fixture.temp.path())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(&stderr_path).unwrap())
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut output = String::new();
        stdout.read_line(&mut output).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let warning = loop {
            let stderr = fs::read_to_string(&stderr_path).unwrap();
            if stderr.contains("could not open the dashboard automatically") {
                break true;
            }
            if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(20));
        };
        let running = child.try_wait().unwrap().is_none();
        let status = fixture
            .command(name)
            .args(["--json", "status"])
            .output()
            .unwrap();
        let stopped = fixture.command(name).arg("stop").output().unwrap();
        let exit = wait(&mut child);
        stdout.read_to_string(&mut output).unwrap();
        let stderr = fs::read_to_string(&stderr_path).unwrap();

        assert!(
            running,
            "{mode} browser opener stopped the environment: {stderr}"
        );
        assert!(
            warning,
            "{mode} browser opener failure was not reported: {stderr}"
        );
        assert!(
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert!(
            stopped.status.success(),
            "{}",
            String::from_utf8_lossy(&stopped.stderr)
        );
        assert!(exit.success(), "{mode}: {exit}: {stderr}");
        let endpoints: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(endpoints["network"], "regtest");
        assert!(
            !fixture
                .command(name)
                .arg("endpoints")
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}

#[test]
fn native_initialization_and_slow_http_have_absolute_deadlines() {
    for mode in ["init-hang", "drip"] {
        let fixture = Fixture::new(mode);
        let start = Instant::now();
        let output = fixture
            .command("deadline")
            .args([
                "start",
                "--native",
                "--no-open",
                "--timeout",
                "1",
                "--runtime-dir",
            ])
            .arg(fixture.temp.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            start.elapsed() < Duration::from_secs(7),
            "{mode}: {:?}",
            start.elapsed()
        );
    }
}

#[test]
fn native_never_adopts_a_foreign_listener() {
    let fixture = Fixture::new("foreign-listener");
    let output = fixture
        .command("foreign-listener")
        .args([
            "start",
            "--native",
            "--no-open",
            "--timeout",
            "1",
            "--runtime-dir",
        ])
        .arg(fixture.temp.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
}
#[test]
fn native_cleans_initializer_and_application_descendants() {
    let fixture = Fixture::new("init-descendant");
    let pidfile = fixture.temp.path().join("application-descendant");
    let script = format!("sleep 300 & echo $! > '{}'", pidfile.display());
    let output = fixture.run("descendants", &["/bin/sh", "-c", &script]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pid: i32 = fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
    assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err());
}
#[test]
fn native_service_exit_stops_application() {
    let fixture = Fixture::new("service-fail");
    let start = Instant::now();
    let output = fixture.run("service-failure", &["/bin/sleep", "30"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("app exited"));
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[test]
fn native_interrupted_startup_removes_state_and_allows_the_same_name() {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    for signal in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP] {
        let fixture = Fixture::new("init-hang");
        let state = fixture.state("cancel-start");
        let mut child = fixture
            .command("cancel-start")
            .args([
                "start",
                "--native",
                "--no-open",
                "--timeout",
                "30",
                "--runtime-dir",
            ])
            .arg(fixture.temp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !state.join("init.pid").exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "startup exited before initialization"
            );
            assert!(Instant::now() < deadline, "initializer did not start");
            thread::sleep(Duration::from_millis(20));
        }
        let pid = fs::read_to_string(state.join("init.pid"))
            .unwrap()
            .parse::<i32>()
            .unwrap();
        kill(Pid::from_raw(child.id() as i32), signal).unwrap();
        let exit = wait(&mut child);
        let stopped = kill(Pid::from_raw(pid), None) == Err(nix::errno::Errno::ESRCH);
        if !stopped {
            // Stop this fixture's group even when testing the old unhandled-HUP bug.
            nix::sys::signal::killpg(Pid::from_raw(pid), Signal::SIGTERM).unwrap();
        }
        assert!(!exit.success());
        assert!(stopped, "{signal}: initializer survived launcher exit");
        assert!(
            !state.exists(),
            "{signal}: interrupted startup retained its instance"
        );

        let server = fixture.temp.path().join("bin/ths-server");
        let script = fs::read_to_string(&server)
            .unwrap()
            .replace("'init-hang'", "'normal'");
        fs::write(server, script).unwrap();
        let output = fixture.run("cancel-start", &["/usr/bin/true"]);
        assert!(
            output.status.success(),
            "{signal}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!state.exists());
    }
}

#[test]
fn native_hangup_stops_services_removes_state_and_preserves_another_instance() {
    use nix::{
        sys::signal::{Signal, kill, killpg},
        unistd::Pid,
    };
    let fixture = Fixture::new("normal");
    let (mut child, endpoints) = fixture.start("hangup");
    let (mut other, _) = fixture.start("survivor");
    let state = fixture.state("hangup");
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(state.join("native.json")).unwrap()).unwrap();
    let pids: Vec<Pid> = record["pids"]
        .as_object()
        .unwrap()
        .values()
        .map(|pid| Pid::from_raw(pid.as_i64().unwrap() as i32))
        .collect();
    kill(Pid::from_raw(child.id() as i32), Signal::SIGHUP).unwrap();
    let exit = wait(&mut child);
    let remaining: Vec<_> = pids
        .into_iter()
        .filter(|pid| kill(*pid, None) != Err(nix::errno::Errno::ESRCH))
        .collect();
    let listeners_closed = ["dashboard", "rpc", "lightwalletd"].into_iter().all(|key| {
        let url = reqwest::Url::parse(endpoints[key].as_str().unwrap()).unwrap();
        std::net::TcpStream::connect(("127.0.0.1", url.port().unwrap())).is_err()
    });
    // The regression must also clean up if the launcher still exits without a handler.
    for pid in &remaining {
        killpg(*pid, Signal::SIGTERM).unwrap();
    }
    let other_running = fixture
        .command("survivor")
        .args(["--json", "status"])
        .output()
        .unwrap();
    let other_stop = fixture.command("survivor").arg("stop").output().unwrap();
    let other_exit = wait(&mut other);

    assert_eq!(exit.code(), Some(129));
    assert!(
        remaining.is_empty(),
        "native services survived SIGHUP: {remaining:?}"
    );
    assert!(
        listeners_closed,
        "native listeners remained open after SIGHUP"
    );
    assert!(!state.exists());
    assert!(other_running.status.success());
    assert!(other_stop.status.success());
    assert!(other_exit.success());
    assert!(fixture.run("hangup", &["/usr/bin/true"]).status.success());
}

#[test]
fn native_stale_state_never_signals_persisted_pids_or_deletes_data() {
    let fixture = Fixture::new("init-fail");
    assert!(!fixture.run("stale", &["/usr/bin/true"]).status.success());
    let output = fixture
        .command("stale")
        .args(["--json", "doctor", "--native", "--runtime-dir"])
        .arg(fixture.temp.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let doctor: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let state = Path::new(doctor["config_dir"].as_str().unwrap()).join("stale");
    fs::write(
        state.join("native.json"),
        serde_json::to_vec(
            &serde_json::json!({"cleaned": false,"pids":{"app":std::process::id()}}),
        )
        .unwrap(),
    )
    .unwrap();
    let output = fixture.command("stale").arg("stop").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("refusing to signal persisted PIDs"));
    assert!(state.exists());
    // Even corrupt main metadata cannot send native recovery into Docker deletion.
    fs::write(state.join("instance.json"), b"invalid").unwrap();
    assert!(
        !fixture
            .command("stale")
            .arg("stop")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(state.exists());
}

#[test]
fn native_name_collision_preserves_the_existing_owner() {
    let fixture = Fixture::new("normal");
    let (mut child, _) = fixture.start("collision");
    let output = fixture.run("collision", &["/usr/bin/true"]);
    assert!(!output.status.success());
    assert!(child.try_wait().unwrap().is_none());
    assert!(
        fixture
            .command("collision")
            .arg("stop")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(wait(&mut child).success());
}

#[test]
fn docker_start_and_run_cannot_delete_abandoned_native_state() {
    use std::os::unix::fs::PermissionsExt;
    for mode in ["start", "run"] {
        let fixture = Fixture::new("init-fail");
        assert!(
            !fixture
                .run("abandoned", &["/usr/bin/true"])
                .status
                .success()
        );
        let output = fixture
            .command("abandoned")
            .args(["--json", "doctor", "--native", "--runtime-dir"])
            .arg(fixture.temp.path())
            .output()
            .unwrap();
        let doctor: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let state = Path::new(doctor["config_dir"].as_str().unwrap()).join("abandoned");
        fs::write(state.join("native.json"), br#"{"cleaned":false,"pids":{}}"#).unwrap();
        fs::write(state.join("wallet/keep"), "wallet data").unwrap();
        let docker = fixture.temp.path().join("docker");
        fs::write(&docker, "#!/bin/sh\ncase \"$1 $2\" in\n'version --format') echo fixture;;\n'image inspect'|'container ls'|'network ls'|'volume ls') exit 0;;\n*) exit 1;;\nesac\n").unwrap();
        fs::set_permissions(docker, fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = fixture.command("abandoned");
        command.env(
            "PATH",
            format!("{}:/usr/bin:/bin", fixture.temp.path().display()),
        );
        if mode == "start" {
            command.args(["--json", "start", "--no-open"]);
        } else {
            command.args(["run", "--", "/usr/bin/true"]);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert_eq!(
            fs::read_to_string(state.join("wallet/keep"))
                .ok()
                .as_deref(),
            Some("wallet data"),
            "Docker {mode} removed native state: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
