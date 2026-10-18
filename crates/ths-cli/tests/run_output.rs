#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

// `ths run` has no port offset, so a fixture that reaches allocation needs the default ports.
const DEFAULT_PORTS: [u16; 4] = [32_805, 18_232, 18_233, 9_067];

fn default_ports_free() -> bool {
    DEFAULT_PORTS
        .iter()
        .all(|port| TcpListener::bind(("127.0.0.1", *port)).is_ok())
}

fn install_fixtures(directory: &Path) {
    for (file, script) in [
        ("docker", include_str!("fixtures/docker.sh")),
        ("curl", include_str!("fixtures/curl.sh")),
    ] {
        let path = directory.join(file);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

// No real Docker resources are touched: `docker` and `curl` are fixture scripts.
fn run_command(directory: &Path, name: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ths"));
    command
        .args(["--name", name, "run", "--timeout", "20", "--"])
        .env("HOME", directory)
        .env("XDG_CONFIG_HOME", directory)
        .env("PATH", format!("{}:/usr/bin:/bin", directory.display()))
        // Runtime status reads honour proxy settings, unlike the readiness probes.
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("THS_TEST_STATE", directory.join("state"))
        .env("THS_TEST_COMMANDS", directory.join("commands"))
        .env("THS_TEST_NAME", name)
        .env("THS_TEST_PORT", "18232")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn respond(mut stream: TcpStream) {
    let mut request = [0; 2048];
    let read = stream.read(&mut request).unwrap_or(0);
    let request = String::from_utf8_lossy(&request[..read]);
    let body = if request.starts_with("GET /api/v1/status ") {
        r#"{"node_height":101,"lightwalletd_height":101,"wallet_scanned_height":101,"wallet_sync_state":"ready","settled":true,"settlement_blocker":null}"#
    } else {
        r#"{"result":"test-tip"}"#
    };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
}

#[test]
fn run_preflight_failure_keeps_stdout_empty() {
    let directory = tempfile::tempdir().unwrap();
    install_fixtures(directory.path());
    let name = format!("run-{}", uuid::Uuid::new_v4().simple());
    let output = run_command(directory.path(), &name)
        .arg("true")
        .env("THS_TEST_MISSING_IMAGE", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.stdout.is_empty(),
        "stdout: {}\nstderr: {stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    for expected in ["Docker test-docker", "required image"] {
        assert!(stderr.contains(expected), "missing {expected:?}: {stderr}");
    }
}

#[test]
fn run_leaves_stdout_to_the_child() {
    if !default_ports_free() {
        eprintln!("skipping: the default ths ports are in use on this host");
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    install_fixtures(directory.path());
    let name = format!("run-{}", uuid::Uuid::new_v4().simple());
    let commands_path = directory.path().join("commands");
    let stop_server = Arc::new(AtomicBool::new(false));
    let server_stop = Arc::clone(&stop_server);
    let server = thread::spawn(move || {
        // Bind only after init, as the real services would, so startup's port check passes.
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
            TcpListener::bind(("127.0.0.1", 18_232)).unwrap(),
            TcpListener::bind(("127.0.0.1", 32_805)).unwrap(),
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
                        stream.set_nonblocking(false).unwrap();
                        respond(stream);
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

    let output = run_command(directory.path(), &name)
        .args(["printf", "CHILD\\n"])
        .output()
        .unwrap();
    stop_server.store(true, Ordering::Relaxed);
    server.join().unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert_eq!(stdout, "CHILD\n", "stderr: {stderr}");
    for expected in [
        "Docker test-docker",
        "Preparing a fresh",
        "running child command",
        "disposable test mnemonic",
        "docker lifecycle output",
    ] {
        assert!(stderr.contains(expected), "missing {expected:?}: {stderr}");
    }
    for kind in ["container", "volume", "network"] {
        let resources = directory.path().join("state").join(kind);
        assert!(
            fs::read_dir(&resources)
                .unwrap_or_else(|error| panic!("reading {}: {error}", resources.display()))
                .next()
                .is_none(),
            "fake {kind} resources were not removed; launcher stderr: {stderr}"
        );
    }
    let commands = fs::read_to_string(directory.path().join("commands")).unwrap();
    for cleanup in ["rm -f", "volume rm", "network rm"] {
        assert!(
            commands.contains(cleanup),
            "missing {cleanup:?}: {commands}"
        );
    }
}
