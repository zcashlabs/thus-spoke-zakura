use std::io;

use tracing::Subscriber;
use tracing_subscriber::{EnvFilter, fmt::MakeWriter, util::SubscriberInitExt};

/// The app container has no terminal, and Docker keeps whatever the process
/// writes. `tracing-subscriber` emits ANSI color codes by default when its
/// `ansi` feature is enabled, unless `NO_COLOR` is a non-empty value. It does
/// not check whether the output is a terminal. Zakura's logs are plain text;
/// these are too.
pub(super) fn init() {
    subscriber(filter_from_env(), io::stdout).init();
}

pub(super) fn subscriber<W>(filter: EnvFilter, writer: W) -> impl Subscriber + Send + Sync
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(writer)
        .finish()
}

fn filter_from_env() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter())
}

pub(super) fn default_filter() -> EnvFilter {
    "ths_server=info,tower_http=info".into()
}

#[cfg(test)]
#[path = "../tests/unit/logging/tests.rs"]
mod tests;
