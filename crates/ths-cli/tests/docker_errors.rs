#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, process::Command};

use tempfile::TempDir;

/// Records an environment so `ths status` goes on to ask Docker whether its
/// app container exists, which is the read that used to report a blank reason.
fn record_instance(directory: &TempDir) {
    let instance_dir = directory.path().join("thus-spoke-zakura").join("default");
    fs::create_dir_all(&instance_dir).unwrap();
    fs::write(
        instance_dir.join("instance.json"),
        r#"{"name":"default","version":1,"endpoints":{"dashboard":"http://127.0.0.1:32805","rpc":"http://127.0.0.1:18232","lightwalletd":"http://127.0.0.1:9067","p2p":"127.0.0.1:18233"}}"#,
    )
    .unwrap();
}

fn fake_docker(directory: &TempDir, script: &str) {
    let path = directory.path().join("docker");
    fs::write(&path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// `status` reads the recorded environment, then asks Docker whether the app
/// container is running.
fn status(directory: &TempDir) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ths"))
        .args(["status"])
        // Isolate only the child's PATH, never the test process or real Docker.
        .env("HOME", directory.path())
        .env("XDG_CONFIG_HOME", directory.path())
        .env("PATH", directory.path())
        .output()
        .unwrap()
}

/// Docker does not always explain itself: a daemon killed mid-call, a signal,
/// or a wrapper script can exit nonzero having printed nothing. The launcher
/// builds this error from Docker's stderr alone, so such a failure used to
/// surface as a bare `Error:` with nothing identifying the failed command.
#[test]
fn silent_docker_failure_reports_the_command_instead_of_a_blank_error() {
    let directory = tempfile::tempdir().unwrap();
    fake_docker(&directory, "#!/bin/sh\nexit 1\n");
    record_instance(&directory);

    let output = status(&directory);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("docker container ls"),
        "the error did not name the failed Docker command: {stderr:?}"
    );
    assert!(
        stderr.contains("failed"),
        "the error did not report the failure: {stderr:?}"
    );
    assert!(
        !stderr.trim().eq_ignore_ascii_case("error:"),
        "a silent Docker failure produced a blank diagnostic: {stderr:?}"
    );
    assert!(
        output.stdout.is_empty(),
        "a failed status wrote to stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Docker's own diagnostic is the most useful part of the message, so the fix
/// must add context without discarding what Docker said.
#[test]
fn docker_stderr_is_retained_alongside_the_failed_command() {
    let directory = tempfile::tempdir().unwrap();
    fake_docker(
        &directory,
        "#!/bin/sh\necho 'Cannot connect to the Docker daemon' >&2\nexit 1\n",
    );
    record_instance(&directory);

    let output = status(&directory);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("docker container ls"),
        "the error did not name the failed Docker command: {stderr:?}"
    );
    assert!(
        stderr.contains("Cannot connect to the Docker daemon"),
        "Docker's own diagnostic was discarded: {stderr:?}"
    );
}
