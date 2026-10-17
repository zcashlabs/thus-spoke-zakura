#[tokio::main]
async fn main() -> anyhow::Result<()> {
    ths_server::init_tracing();
    match ths_server::run_from(std::env::args_os()).await {
        Ok(()) => Ok(()),
        Err(error) => match error.downcast::<clap::Error>() {
            Ok(error) => error.exit(),
            Err(error) => Err(error),
        },
    }
}
