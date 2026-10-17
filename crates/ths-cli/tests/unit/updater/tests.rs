use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

use super::*;

#[test]
fn parses_prefixed_versions_and_compares_all_states() {
    let current = parse_version("v1.2.3").unwrap();
    assert_eq!(
        compare_versions(&current, &parse_version("1.2.4").unwrap()),
        Status::UpdateAvailable
    );
    assert_eq!(
        compare_versions(&current, &parse_version("1.2.3").unwrap()),
        Status::UpToDate
    );
    assert_eq!(
        compare_versions(&current, &parse_version("1.2.2").unwrap()),
        Status::Ahead
    );
    assert!(parse_version("latest").is_err());
    assert!(parse_version("1.2.3-beta.1").is_err());
}

#[test]
fn check_uses_a_distinct_update_available_exit_code() {
    assert_eq!(check_exit_code(Status::UpToDate), ExitCode::SUCCESS);
    assert_eq!(check_exit_code(Status::Ahead), ExitCode::SUCCESS);
    assert_eq!(check_exit_code(Status::UpdateAvailable), ExitCode::from(10));
}

#[test]
fn discovers_latest_release_and_rejects_http_errors() {
    let endpoint = serve_once("200 OK", r#"{"tag_name":"v2.3.4"}"#);
    assert_eq!(
        latest_version_from(&endpoint).unwrap(),
        Version::new(2, 3, 4)
    );

    let endpoint = serve_once("403 Forbidden", r#"{"message":"rate limited"}"#);
    let error = latest_version_from(&endpoint).unwrap_err().to_string();
    assert!(error.contains("HTTP 403"));
}

#[test]
fn startup_notice_is_actionable_and_fail_open() {
    let current = Version::new(2, 3, 3);
    let endpoint = serve_once("200 OK", r#"{"tag_name":"v2.3.4"}"#);
    assert_eq!(
        startup_notice_from(&endpoint, &current).as_deref(),
        Some("Update available: 2.3.3 → 2.3.4. Run `ths update` to install it.")
    );

    let current = Version::new(2, 3, 4);
    let endpoint = serve_once("200 OK", r#"{"tag_name":"v2.3.4"}"#);
    assert!(startup_notice_from(&endpoint, &current).is_none());

    let endpoint = serve_once("503 Service Unavailable", "unavailable");
    assert!(startup_notice_from(&endpoint, &current).is_none());
}

#[test]
fn embedded_installer_receives_exact_version_and_destination() {
    let version = Version::new(4, 5, 6);
    run_installer(
        "test \"$THS_VERSION\" = 4.5.6 && test \"$THS_INSTALL_DIR\" = /tmp/ths\n",
        &version,
        Path::new("/tmp/ths"),
        false,
    )
    .unwrap();
    assert!(run_installer("exit 7\n", &version, Path::new("/tmp/ths"), false).is_err());
}

#[test]
fn source_builds_cannot_mutate_themselves() {
    if !cfg!(feature = "release-distribution") {
        assert!(ensure_release_distribution().is_err());
    }
}

#[test]
fn uninstall_removes_only_an_executable_named_ths() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("ths");
    std::fs::write(&executable, "launcher").unwrap();

    remove_installed_executable(&executable).unwrap();
    assert!(!executable.exists());

    let unexpected = directory.path().join("another-tool");
    std::fs::write(&unexpected, "keep me").unwrap();
    assert!(remove_installed_executable(&unexpected).is_err());
    assert!(unexpected.exists());
}

#[cfg(unix)]
#[test]
fn update_and_uninstall_act_on_the_launcher_behind_a_symlink() {
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real");
    let link = directory.path().join("link");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::create_dir_all(&link).unwrap();
    let launcher = real.join("ths");
    std::fs::write(&launcher, "0.3.0").unwrap();
    std::os::unix::fs::symlink(&launcher, link.join("ths")).unwrap();

    let resolved = resolve_launcher(&link.join("ths")).unwrap();
    assert_eq!(resolved, std::fs::canonicalize(&launcher).unwrap());

    // The installer replaces `$THS_INSTALL_DIR/ths` with `mv -f`, which would turn a symlink
    // into a regular file.
    run_installer(
        "printf 0.2.1 > \"$THS_INSTALL_DIR/ths.new\" && mv -f \"$THS_INSTALL_DIR/ths.new\" \"$THS_INSTALL_DIR/ths\"\n",
        &Version::new(0, 2, 1),
        resolved.parent().unwrap(),
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(&launcher).unwrap(), "0.2.1");
    assert!(
        std::fs::symlink_metadata(link.join("ths"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    remove_installed_executable(&resolved).unwrap();
    assert!(!launcher.exists());
    assert!(
        std::fs::symlink_metadata(link.join("ths"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

fn serve_once(status: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 2048];
        let _ = stream.read(&mut request);
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    format!("http://{address}")
}
