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
