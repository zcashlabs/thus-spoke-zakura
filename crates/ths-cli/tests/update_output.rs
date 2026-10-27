#![cfg(all(unix, feature = "release-distribution"))]

use std::{fs, os::unix::fs::PermissionsExt, process::Command};

fn update(json: bool, fail: bool) -> std::process::Output {
    let directory = tempfile::tempdir().unwrap();
    let shell = directory.path().join("sh");
    // Consume, but never execute, the embedded installer: no downloads or replacement.
    fs::write(
        &shell,
        "#!/bin/sh\ncat >/dev/null\nprintf 'installer progress for %s\\n' \"$THS_VERSION\"\nprintf 'installer diagnostic\\n' >&2\nexit \"$THS_TEST_INSTALL_EXIT\"\n",
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_ths"));
    if json {
        command.arg("--json");
    }
    command
        .args(["update", "0.0.1"])
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", directory.path().display()),
        )
        .env("THS_TEST_INSTALL_EXIT", if fail { "7" } else { "0" })
        .output()
        .unwrap()
}

#[test]
fn json_install_prints_one_report_and_keeps_installer_output_on_stderr() {
    let output = update(true, false);
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({
            "current": "0.0.1", "available": "0.0.1", "status": "up_to_date"
        })
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("installer progress for 0.0.1\n"));
    assert!(stderr.contains("installer diagnostic\n"));
}

#[test]
fn human_install_keeps_the_installers_output() {
    let output = update(false, false);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"installer progress for 0.0.1\n");
    assert_eq!(output.stderr, b"installer diagnostic\n");
}

#[test]
fn failed_json_install_does_not_print_a_success_report() {
    let output = update(true, true);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("installer progress for 0.0.1\n"));
    assert!(stderr.contains("installer diagnostic\n"));
    assert!(stderr.contains("the verified installer failed"));
}
