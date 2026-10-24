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

/// Regression for #251: the server initializer must disable ANSI so Docker
/// does not store color codes in `ths logs app` output.
#[test]
fn server_tracing_emits_plain_text() {
    let output = LogWriter::default();
    let writer = output.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ths_server=info,tower_http=info".into()),
        )
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        tracing::error!("dashboard ready");
        tracing::warn!("dashboard ready");
        tracing::info!("dashboard ready");
    });

    let logs = String::from_utf8(output.0.lock().expect("log buffer").clone()).expect("utf-8");
    assert!(logs.contains("ERROR"), "{logs}");
    assert!(logs.contains("WARN"), "{logs}");
    assert!(logs.contains("INFO"), "{logs}");
    assert!(
        !logs
            .chars()
            .any(|character| character == '\u{1b}' || character == '\u{9b}'),
        "log line contains a terminal escape: {logs:?}"
    );
}
