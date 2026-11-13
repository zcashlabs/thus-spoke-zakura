use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ths-server", version)]
pub(crate) struct Args {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    Init {
        #[arg(long, default_value = "/data")]
        data_dir: PathBuf,
        #[arg(long, default_value = "/config")]
        config_dir: PathBuf,
        #[command(flatten)]
        native: NativeInit,
        /// Height at which NU7 activates. Fixed for the life of the instance.
        #[arg(
            long,
            default_value_t = crate::network::DEFAULT_NU7_ACTIVATION_HEIGHT,
            value_parser = clap::value_parser!(u32).range(1..)
        )]
        nu7_height: u32,
    },
    Serve {
        #[arg(long, default_value = "/data")]
        data_dir: PathBuf,
    },
}

#[derive(clap::Args, Default)]
pub(crate) struct NativeInit {
    #[arg(long, requires_all = ["native_rpc_port", "native_p2p_port"])]
    pub(crate) native_chain_dir: Option<PathBuf>,
    #[arg(long, requires = "native_chain_dir", value_parser = clap::value_parser!(u16).range(1..))]
    pub(crate) native_rpc_port: Option<u16>,
    #[arg(long, requires = "native_chain_dir", value_parser = clap::value_parser!(u16).range(1..))]
    pub(crate) native_p2p_port: Option<u16>,
}
