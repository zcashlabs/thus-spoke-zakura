#[test]
fn help_and_invalid_arguments_return_typed_errors() {
    for (argv, expected) in [
        (vec!["ths", "--help"], clap::error::ErrorKind::DisplayHelp),
        (
            vec!["ths", "--version"],
            clap::error::ErrorKind::DisplayVersion,
        ),
        (
            vec!["ths", "--not-a-ths-option"],
            clap::error::ErrorKind::UnknownArgument,
        ),
    ] {
        let error = thus_spoke_zakura::run_from(argv)
            .expect_err("the runner must return argument processing to its caller")
            .downcast::<clap::Error>()
            .expect("preserve the Clap error type");
        assert_eq!(error.kind(), expected);
    }
}

#[test]
fn binary_preserves_help_and_parse_error_exits() {
    let binary = env!("CARGO_BIN_EXE_ths");
    let help = std::process::Command::new(binary)
        .arg("--help")
        .output()
        .unwrap();
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage:"));
    assert!(help.stderr.is_empty());
    let invalid = std::process::Command::new(binary)
        .arg("--not-a-ths-option")
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("unexpected argument"));
}

#[test]
fn completions_zsh_prints_a_script_without_touching_an_environment() {
    let binary = env!("CARGO_BIN_EXE_ths");
    let output = std::process::Command::new(binary)
        .args(["completions", "zsh"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let script = String::from_utf8(output.stdout).unwrap();
    assert!(script.contains("#compdef ths"));
    for command in ["start", "wallet", "completions", "--name", "--json"] {
        assert!(
            script.contains(command),
            "zsh completion script is missing {command}"
        );
    }

    let unknown = std::process::Command::new(binary)
        .args(["completions", "powershell"])
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(2));
    assert!(unknown.stdout.is_empty());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("invalid value"));
}

#[test]
fn completions_do_not_panic_when_stdout_closes_early() {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_ths"))
        .args(["completions", "zsh"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut first = [0; 16];
    assert_eq!(std::io::Read::read(&mut stdout, &mut first).unwrap(), 16);
    drop(stdout);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn json_run_reaches_dispatch_before_runtime_discovery() {
    let error = thus_spoke_zakura::run_from(["ths", "--json", "run", "--", "echo", "ready"])
        .expect_err("run owns stdout");
    assert!(
        error
            .to_string()
            .contains("--json cannot be used with `run`"),
        "{error:#}"
    );
    assert!(error.downcast_ref::<clap::Error>().is_none());
}

#[cfg(unix)]
#[test]
fn binary_ends_quietly_when_the_reader_closes_stdout() {
    use std::os::unix::{fs::PermissionsExt, process::ExitStatusExt};

    let directory = tempfile::tempdir().unwrap();
    let docker = directory.path().join("docker");
    std::fs::write(&docker, include_str!("fixtures/docker.sh")).unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ths"))
        .arg("list")
        .env("HOME", directory.path())
        .env("XDG_CONFIG_HOME", directory.path())
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", directory.path().display()),
        )
        .env("THS_TEST_STATE", directory.path().join("state"))
        .env("THS_TEST_COMMANDS", directory.path().join("commands"))
        .env("THS_TEST_NAME", "closed-pipe")
        .env("THS_TEST_PORT", "18232")
        .stdout(writer)
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.signal(), Some(libc::SIGPIPE), "{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
}
