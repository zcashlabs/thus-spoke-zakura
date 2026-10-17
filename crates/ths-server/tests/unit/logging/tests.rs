use super::*;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture(filter: EnvFilter, emit: impl FnOnce()) -> String {
    let output = LogWriter::default();
    let writer = output.clone();
    let subscriber = subscriber(filter, move || writer.clone());
    tracing::subscriber::with_default(subscriber, emit);
    String::from_utf8(output.0.lock().expect("log buffer").clone()).expect("log text is utf-8")
}

fn assert_plain(logs: &str) {
    assert!(
        !logs
            .chars()
            .any(|character| character == '\u{1b}' || character == '\u{9b}'),
        "log line contains a terminal escape: {logs:?}"
    );
}

#[test]
fn server_logs_are_plain_text_at_every_level() {
    let logs = capture("trace".into(), || {
        tracing::error!("dashboard ready");
        tracing::warn!("dashboard ready");
        tracing::info!("dashboard ready");
        tracing::debug!("dashboard ready");
        tracing::trace!("dashboard ready");
    });
    let lines: Vec<_> = logs.lines().collect();
    assert_eq!(lines.len(), 5, "{logs}");
    for level in ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"] {
        assert!(
            lines
                .iter()
                .any(|line| line.contains(level) && line.contains("dashboard ready")),
            "missing {level} in {logs}"
        );
    }
    assert_plain(&logs);
    assert!(
        lines
            .iter()
            .all(|line| line.contains('T') && line.contains('Z')),
        "expected a timestamp on each line: {logs}"
    );
}

#[test]
fn empty_and_whitespace_messages_stay_plain_text() {
    let logs = capture("info".into(), || {
        tracing::info!("");
        tracing::info!("   ");
        tracing::info!(address = "", "serving stored status");
        let missing: Option<&str> = None;
        tracing::info!(address = missing, "serving stored status");
    });
    let lines: Vec<_> = logs.lines().collect();
    assert_eq!(lines.len(), 4, "{logs}");
    assert!(lines.iter().all(|line| line.contains("INFO")), "{logs}");
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("serving stored status"))
            .count(),
        2,
        "{logs}"
    );
    assert!(logs.contains("address="), "{logs}");
    assert_plain(&logs);
}

#[test]
fn logged_escape_sequences_are_not_passed_through() {
    let logs = capture("info".into(), || {
        tracing::info!("prefix \u{1b}[31mred\u{1b}[0m suffix");
        tracing::info!(target: "ths_server", "csi \u{9b}31m");
        tracing::info!("line-one\nline-two");
    });
    assert!(logs.contains("prefix"), "{logs}");
    assert!(logs.contains("red"), "{logs}");
    assert!(logs.contains("suffix"), "{logs}");
    assert!(logs.contains("line-one"), "{logs}");
    assert!(logs.contains("line-two"), "{logs}");
    assert_plain(&logs);
}

#[test]
fn default_filter_keeps_server_info_and_drops_quieter_or_unrelated_events() {
    let logs = capture(default_filter(), || {
        tracing::debug!("hidden debug");
        tracing::trace!(target: "tower_http", "too quiet");
        tracing::info!(target: "other_crate", "foreign info");
        tracing::info!("visible info");
        tracing::warn!(target: "tower_http::trace", "proxy warning");
    });
    assert!(!logs.contains("hidden debug"), "{logs}");
    assert!(!logs.contains("too quiet"), "{logs}");
    assert!(!logs.contains("foreign info"), "{logs}");
    assert!(logs.contains("visible info"), "{logs}");
    assert!(logs.contains("proxy warning"), "{logs}");
    assert_eq!(logs.lines().count(), 2, "{logs}");
    assert_plain(&logs);
}

#[test]
fn no_events_leave_the_log_empty() {
    let logs = capture(default_filter(), || {});
    assert_eq!(logs, "");
}
