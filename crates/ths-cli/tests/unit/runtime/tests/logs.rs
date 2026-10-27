use super::*;

#[test]
fn builds_docker_logs_arguments() {
    assert_eq!(
        logs_args("ths-default-app", false, None),
        ["logs", "ths-default-app"]
    );
    assert_eq!(
        logs_args("ths-default-zakura", true, None),
        ["logs", "--follow", "ths-default-zakura"]
    );
    assert_eq!(
        logs_args("ths-default-app", false, Some(200)),
        ["logs", "--tail", "200", "ths-default-app"]
    );
    assert_eq!(
        logs_args("ths-default-lightwalletd", true, Some(200)),
        [
            "logs",
            "--follow",
            "--tail",
            "200",
            "ths-default-lightwalletd"
        ]
    );
}

#[test]
fn copies_only_the_requested_leading_lines() {
    let mut out = Vec::new();
    let mut input = std::io::Cursor::new("one\ntwo\nthree\n");
    assert_eq!(copy_lines(&mut input, &mut out, 2).unwrap(), 2);
    assert_eq!(out, b"one\ntwo\n");
    assert_eq!(input.position(), 8, "read past the requested lines");

    let mut out = Vec::new();
    let input = std::io::Cursor::new("one\ntwo");
    assert_eq!(copy_lines(input, &mut out, 5).unwrap(), 2);
    assert_eq!(out, b"one\ntwo");
}

#[test]
fn a_closed_reader_ends_copying_without_a_failure() {
    struct Closed;
    impl std::io::Write for Closed {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let copied = copy_lines(std::io::Cursor::new("one\ntwo\n"), Closed, 5);
    assert!(matches!(
        copied_log_lines(copied).unwrap(),
        LogCopy::ReaderClosed
    ));
    assert!(matches!(
        copied_log_lines(Ok(2)).unwrap(),
        LogCopy::Printed(2)
    ));
    let error = copied_log_lines(Err(std::io::Error::other("disk full"))).unwrap_err();
    assert!(!error.to_string().contains("Broken pipe"), "{error}");
}

#[test]
fn a_closed_pipe_is_not_a_failed_docker_logs_command() {
    let closed = [status_of("kill -PIPE $$"), status_of("exit 141")];
    for status in closed {
        assert!(
            !docker_logs_failed(&status),
            "closed pipe reported as failure: {status:?}"
        );
    }

    assert!(docker_logs_failed(&status_of("exit 1")));
    assert!(!docker_logs_failed(&status_of("exit 0")));
}

fn status_of(script: &str) -> std::process::ExitStatus {
    std::process::Command::new("sh")
        .args(["-c", script])
        .status()
        .unwrap()
}
