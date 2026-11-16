use super::*;
use std::os::unix::process::ExitStatusExt;

/// `from_raw` takes a raw wait status, so an exit code of 1 is 1 << 8.
const EXIT_ONE: i32 = 0x0100;

/// A Docker failure that printed nothing must still name the command. Building
/// the message only from stderr made every such error an empty string, so
/// callers surfaced a blank diagnostic with no way to tell which probe failed.
#[test]
fn silent_docker_failure_names_the_command_and_its_status() {
    let message = docker_failure_message(
        &["container", "ls", "-a", "--no-trunc", "--format", "{{.ID}}"],
        &ExitStatus::from_raw(EXIT_ONE),
        b"",
    );
    assert!(
        message.contains("docker container ls -a --no-trunc --format {{.ID}} failed"),
        "message did not name the failed command: {message}"
    );
    assert!(
        !message.trim().is_empty(),
        "a silent failure must not produce an empty message"
    );
}

/// Whitespace-only stderr is as useless as none at all, so it must not turn
/// the message into a bare trailing separator.
#[test]
fn whitespace_only_docker_stderr_is_treated_as_no_diagnostic() {
    let message =
        docker_failure_message(&["version"], &ExitStatus::from_raw(EXIT_ONE), b" \n\t\r\n");
    assert!(
        !message.trim_end().ends_with(':'),
        "whitespace stderr left a dangling separator: {message}"
    );
    assert!(message.contains("failed"), "{message}");
}

/// Docker's own diagnostic is the most useful part of the message, so keep it
/// verbatim after the command and status.
#[test]
fn docker_stderr_is_retained_alongside_the_command_and_status() {
    let message = docker_failure_message(
        &["container", "inspect", "abc"],
        &ExitStatus::from_raw(EXIT_ONE),
        b"Error: No such container: abc\n",
    );
    assert!(
        message.contains("docker container inspect abc failed"),
        "{message}"
    );
    assert!(message.contains("No such container: abc"), "{message}");
}

/// Cleanup embeds these messages in retained recovery reasons, so a failure
/// with no stderr must still carry the identity of the command that failed.
#[test]
fn docker_failure_message_survives_non_utf8_stderr() {
    let message = docker_failure_message(
        &["volume", "ls"],
        &ExitStatus::from_raw(EXIT_ONE),
        &[0xff, 0xfe],
    );
    assert!(message.contains("docker volume ls failed"), "{message}");
}

/// Writes a stub that stands in for `docker`, then runs `test` against it so a
/// real invocation can be driven without a daemon or a process-wide PATH change.
#[cfg(unix)]
pub(super) fn with_stub_docker<T>(body: &str, test: impl FnOnce() -> T) -> T {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("docker");
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    with_docker_program(path, test)
}

/// `docker_output_args` is the function every Docker read goes through, so it
/// is what has to name the command. Driving the real function keeps this
/// regression test independent of which caller happens to surface the error.
#[cfg(unix)]
#[test]
fn docker_output_args_names_the_command_when_docker_prints_nothing() {
    let error = with_stub_docker("#!/bin/sh\nexit 1\n", || {
        docker_output_args(&["container", "ls", "--quiet"]).unwrap_err()
    })
    .to_string();
    assert!(
        error.contains("docker container ls --quiet failed"),
        "a silent Docker failure did not name the command: {error:?}"
    );
    assert!(
        !error.trim().is_empty(),
        "a silent Docker failure produced an empty message"
    );
}

/// The fix must add context without discarding what Docker actually said.
#[cfg(unix)]
#[test]
fn docker_output_args_keeps_docker_stderr_next_to_the_failed_command() {
    let error = with_stub_docker(
        "#!/bin/sh\necho 'Cannot connect to the Docker daemon' >&2\nexit 1\n",
        || docker_output_args(&["volume", "ls", "--format", "{{.Name}}"]).unwrap_err(),
    )
    .to_string();
    assert!(
        error.contains("docker volume ls --format {{.Name}} failed"),
        "the error did not name the failed command: {error:?}"
    );
    assert!(
        error.contains("Cannot connect to the Docker daemon"),
        "Docker's own diagnostic was discarded: {error:?}"
    );
}

/// A successful read must still return Docker's stdout unchanged.
#[cfg(unix)]
#[test]
fn docker_output_args_still_returns_stdout_on_success() {
    let output = with_stub_docker("#!/bin/sh\nprintf 'name-a\\nname-b\\n'\n", || {
        docker_output_args(&["container", "ls", "--format", "{{.Names}}"]).unwrap()
    });
    assert_eq!(output, "name-a\nname-b");
}
