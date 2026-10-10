#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, process::Command};

use serde_json::Value;
use tempfile::TempDir;

fn docker_fixture(reachable: bool) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let script = if reachable {
        "#!/bin/sh\nprintf '%s\\n' '27.0.0'\n"
    } else {
        "#!/bin/sh\nprintf '%s\\n' 'Docker daemon unavailable' >&2\nexit 1\n"
    };
    let path = dir.path().join("docker");
    fs::write(&path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn doctor(dir: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ths"))
        .args(args)
        // Isolate only the child's PATH, never the test process or real Docker.
        .env("PATH", dir.path())
        .output()
        .unwrap()
}

fn assert_failed_json(output: &std::process::Output) {
    assert_eq!(output.status.code(), Some(1));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["ok"], false);
    assert!(result["docker"].is_null());
    assert!(result["config_dir"].as_str().is_some());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Docker is not reachable"));
}

#[test]
fn doctor_json_reports_unreachable_docker_with_a_failing_exit_status() {
    let dir = docker_fixture(false);
    assert_failed_json(&doctor(&dir, &["--json", "doctor"]));
}

#[test]
fn doctor_json_reports_missing_docker_with_a_failing_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    assert_failed_json(&doctor(&dir, &["doctor", "--json"]));
}

#[test]
fn doctor_json_reports_reachable_docker_with_a_successful_exit_status() {
    let dir = docker_fixture(true);
    let output = doctor(&dir, &["doctor", "--json"]);
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["docker"], "27.0.0");
    assert!(result["config_dir"].as_str().is_some());
    assert!(output.stderr.is_empty());
}

#[test]
fn doctor_human_output_preserves_reachable_and_unreachable_exit_status() {
    let ready = docker_fixture(true);
    let output = doctor(&ready, &["doctor"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Docker 27.0.0"));
    assert!(stdout.contains("Config:"));
    assert!(output.stderr.is_empty());

    let unavailable = docker_fixture(false);
    let output = doctor(&unavailable, &["doctor"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Docker is not reachable"));
}
