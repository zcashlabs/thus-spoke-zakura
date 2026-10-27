#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The app container has no terminal, and Docker keeps whatever the process
    // writes. tracing-subscriber emits ANSI color codes by default when its
    // `ansi` feature is enabled, unless NO_COLOR is set. It does not check
    // whether stdout is a terminal.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ths_server=info,tower_http=info".into()),
        )
        .with_ansi(false)
        .init();
    match ths_server::run_from(std::env::args_os()).await {
        Ok(()) => Ok(()),
        Err(error) => match error.downcast::<clap::Error>() {
            Ok(error) => error.exit(),
            Err(error) => Err(error),
        },
    }
}

#[cfg(test)]
#[path = "../tests/unit/main/tests.rs"]
mod tests;
