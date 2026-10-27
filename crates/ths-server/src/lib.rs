mod api;
mod args;
mod bootstrap;
mod chain;
mod db;
mod mining;
mod network;
mod payments;
mod reconcile;
mod rpc;
mod state;
mod sync;
mod wallet;

pub async fn run_from<I, T>(argv: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    use clap::Parser;
    match args::Args::try_parse_from(argv)?.command {
        args::Command::Init {
            data_dir,
            config_dir,
        } => bootstrap::init(data_dir, config_dir),
        args::Command::Serve { data_dir } => bootstrap::serve(data_dir).await,
    }
}
