mod amount;

use std::{ffi::OsString, path::PathBuf};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use crate::runtime::InstanceName;
pub(crate) use amount::{SendAmount, ZecAmount};

#[derive(Parser)]
#[command(
    name = "ths",
    version,
    about = "A one-command Zakura regtest environment"
)]
pub(crate) struct Cli {
    /// Isolated environment name.
    #[arg(long, global = true)]
    pub(crate) name: Option<InstanceName>,
    /// Print machine-readable output where supported.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Start an environment in the foreground; interrupting deletes it.
    Start {
        #[arg(long)]
        no_open: bool,
        /// Add this many to the default loopback ports. Must be a multiple of 10.
        #[arg(long, default_value_t = 0)]
        port_offset: u16,
        /// Mine a block every SECS seconds, even when there are no transactions.
        #[arg(
            long,
            value_name = "SECS",
            value_parser = clap::value_parser!(u64).range(1..),
            conflicts_with_all = ["mine_on_mempool", "no_auto_mine"]
        )]
        block_time: Option<u64>,
        /// Mine a block whenever the mempool holds new transactions.
        #[arg(long, conflicts_with = "no_auto_mine")]
        mine_on_mempool: bool,
        /// Never mine automatically; confirmed work waits for `ths mine`.
        #[arg(long)]
        no_auto_mine: bool,
    },
    /// Wait until an existing environment is settled.
    Wait {
        /// Maximum time for settlement, in seconds.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Run a command in a fresh, settled environment and delete it afterward.
    Run {
        /// Maximum time for startup and settlement, in seconds.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        /// Command and arguments to execute.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
    /// Build the runtime images from the current source.
    Build {
        /// Keep workspace Rust code unoptimized while optimizing dependencies.
        #[arg(long)]
        dev: bool,
    },
    /// Pull the exact runtime images for this launcher version.
    Pull,
    /// Check for or install a released launcher version.
    Update {
        /// Exact version to install, including an intentional rollback.
        #[arg(value_name = "VERSION", conflicts_with = "check")]
        version: Option<String>,
        /// Only report whether a newer stable release is available.
        #[arg(long)]
        check: bool,
    },
    /// Remove this installed launcher executable.
    Uninstall,
    /// Show service and endpoint status.
    Status,
    /// Open the dashboard in the default browser.
    Open,
    /// Print endpoints for developer tooling.
    Endpoints {
        /// Print export lines for the variables ths run passes to its child.
        #[arg(long)]
        env: bool,
    },
    /// Call a method on the instance's Zakura JSON-RPC endpoint.
    Rpc {
        /// Print the full JSON-RPC response.
        #[arg(long)]
        raw: bool,
        /// Allow `stop`, which shuts the Zakura node down.
        #[arg(long)]
        yes: bool,
        /// JSON-RPC method name.
        method: String,
        /// JSON-RPC parameters.
        ///
        /// Numbers, `true`, `false`, `null`, objects, and arrays are parsed as JSON.
        /// Any other value is sent as a string.
        #[arg(value_name = "PARAMS", allow_hyphen_values = true)]
        params: Vec<String>,
    },
    /// Show a transaction from the running environment's explorer.
    Tx {
        /// Transaction ID.
        txid: String,
    },
    /// Show a block from the explorer, defaulting to the chain tip.
    Block {
        /// Block height or hash.
        id: Option<String>,
    },
    /// Mine blocks on a running development environment.
    Mine {
        /// Number of blocks to mine.
        #[arg(value_parser = clap::value_parser!(u32).range(1..=10_000))]
        blocks: u32,
    },
    /// Replace recent Regtest blocks with a new branch.
    Reorg {
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=1000))]
        depth: u32,
        /// Attempt to include the dropped transactions in replacement blocks.
        #[arg(long)]
        keep_txs: bool,
    },
    /// Remove recent Regtest blocks without mining replacements.
    Rollback {
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=1000))]
        depth: u32,
    },
    /// Send disposable Regtest ZEC to a unified or transparent address.
    Faucet {
        /// Regtest unified or transparent destination address.
        address: String,
        /// Amount of ZEC to send (maximum 5, up to 8 decimal places).
        #[arg(long, default_value = "1")]
        amount: ZecAmount,
    },
    /// Act on the development wallet held by the running ths server.
    Wallet {
        #[command(subcommand)]
        action: WalletCommand,
    },
    /// Stream or print service logs.
    Logs {
        #[arg(value_parser = ["app", "zakura", "lightwalletd"])]
        service: Option<String>,
        #[arg(short, long)]
        follow: bool,
        /// Print the last LINES lines.
        #[arg(long, value_name = "LINES", value_parser = clap::value_parser!(u64).range(1..))]
        tail: Option<u64>,
        /// Print the first LINES lines and exit.
        #[arg(
            long,
            value_name = "LINES",
            value_parser = clap::value_parser!(u64).range(1..),
            conflicts_with_all = ["tail", "follow"]
        )]
        head: Option<u64>,
    },
    /// Stop and delete an environment.
    Stop,
    /// Delete one environment and all of its volumes.
    Reset {
        #[arg(long)]
        force: bool,
    },
    /// List known environments.
    List,
    /// Check local Docker and configuration prerequisites.
    Doctor,
    /// Print a shell completion script.
    Completions {
        /// Shell that will load the script.
        #[arg(value_enum)]
        shell: CompletionShell,
    },
}

#[derive(Subcommand)]
pub(crate) enum WalletCommand {
    /// Show current account addresses and spendable balances.
    Balances,
    /// Print recent wallet transfers and faucet activity.
    Activity {
        /// Maximum number of entries to print (1-100).
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=100))]
        limit: Option<u32>,
    },
    /// Show an account's spendable funds, fee, and maximum send for one pool
    /// route, without sending.
    Quote {
        /// Account index to quote (1-5).
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=5))]
        from: u8,
        /// Pool the quoted send would spend from.
        #[arg(long = "source-pool", value_enum)]
        source_pool: Pool,
        /// Pool the quoted send would pay into. The fee depends on it.
        #[arg(long = "destination-pool", value_enum)]
        destination_pool: Pool,
    },
    /// Send faucet funds from the treasury to one or more accounts.
    Faucet {
        /// Account indices to fund (1-5), e.g. --accounts 1,2,3,5.
        #[arg(
            long,
            required = true,
            value_delimiter = ',',
            value_parser = clap::value_parser!(u8).range(1..=5)
        )]
        accounts: Vec<u8>,
        /// Amount of ZEC to send to each account (maximum 5, up to 8 decimal places).
        #[arg(long, default_value = "1")]
        amount: ZecAmount,
        /// Pool to fund.
        #[arg(long, value_enum, default_value = "ironwood")]
        pool: Pool,
    },
    /// Send funds between development accounts or pools.
    Send(SendArgs),
    /// Spend transparent funds into an account's ironwood balance.
    Shield {
        /// Account index to shield from (1-5).
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=5))]
        from: u8,
        /// Destination account index (1-5); may match --from.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=5))]
        to: u8,
        /// Amount of ZEC to shield, up to 8 decimal places.
        #[arg(long, required_unless_present = "max", conflicts_with = "max")]
        amount: Option<SendAmount>,
        /// Shield the maximum spendable transparent balance after fees.
        #[arg(long, required_unless_present = "amount", conflicts_with = "amount")]
        max: bool,
        /// Text memo for the recipient (up to 512 bytes).
        #[arg(long, value_parser = parse_memo)]
        memo: Option<String>,
    },
    /// Spend ironwood funds into an account's transparent balance.
    Unshield {
        /// Account index to unshield from (1-5).
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=5))]
        from: u8,
        /// Destination account index (1-5); may match --from.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=5))]
        to: u8,
        /// Amount of ZEC to unshield, up to 8 decimal places.
        #[arg(long, required_unless_present = "max", conflicts_with = "max")]
        amount: Option<SendAmount>,
        /// Unshield the maximum spendable Ironwood balance after fees.
        #[arg(long, required_unless_present = "amount", conflicts_with = "amount")]
        max: bool,
    },
}

#[derive(clap::Args)]
pub(crate) struct SendArgs {
    /// Source account index (1-5).
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=5))]
    pub(crate) from: u8,
    /// Destination account index (1-5); may match --from when pools differ.
    #[arg(
        long,
        value_parser = clap::value_parser!(u8).range(1..=5),
        required_unless_present_any = ["uri", "to_address"],
        conflicts_with_all = ["uri", "to_address"]
    )]
    pub(crate) to: Option<u8>,
    /// Regtest unified or transparent address to pay instead of --to. The
    /// destination pool follows the address.
    #[arg(
        long = "to-address",
        value_name = "ADDRESS",
        conflicts_with_all = ["uri", "destination_pool"]
    )]
    pub(crate) to_address: Option<String>,
    /// ZIP-321 payment URI to pay instead of --to. It sets the destination,
    /// pool, and memo, and the amount unless it omits one. A multi-recipient URI
    /// pays every recipient in one transaction and must set each amount.
    #[arg(long, conflicts_with_all = ["destination_pool", "memo"])]
    pub(crate) uri: Option<String>,
    /// Amount of ZEC to send, up to 8 decimal places. With --uri, only for a
    /// URI that does not set an amount.
    #[arg(
        long,
        required_unless_present_any = ["uri", "max"],
        conflicts_with = "max"
    )]
    pub(crate) amount: Option<SendAmount>,
    /// Send the maximum spendable source-pool balance after fees.
    #[arg(
        long,
        required_unless_present_any = ["uri", "amount"],
        conflicts_with = "amount"
    )]
    pub(crate) max: bool,
    /// Pool to spend from.
    #[arg(long = "source-pool", value_enum, default_value = "ironwood")]
    pub(crate) source_pool: Pool,
    /// Pool the destination account receives into.
    #[arg(long = "destination-pool", value_enum, default_value = "ironwood")]
    pub(crate) destination_pool: Pool,
    /// Text memo for the recipient (up to 512 bytes; ironwood destinations only).
    #[arg(long, value_parser = parse_memo)]
    pub(crate) memo: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub(crate) enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

impl CompletionShell {
    pub(crate) fn generator(self) -> clap_complete::Shell {
        match self {
            Self::Bash => clap_complete::Shell::Bash,
            Self::Zsh => clap_complete::Shell::Zsh,
            Self::Fish => clap_complete::Shell::Fish,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub(crate) enum Pool {
    Ironwood,
    Transparent,
}

impl Pool {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Pool::Ironwood => "ironwood",
            Pool::Transparent => "transparent",
        }
    }
}

fn _assert_pathbuf_send(_: PathBuf) {}

/// `--memo ""` is allowed and sends an explicit empty text memo.
fn parse_memo(value: &str) -> Result<String> {
    if value.len() > 512 {
        bail!("memo is {} bytes; the maximum is 512", value.len());
    }
    if value.ends_with('\0') {
        bail!("memo must not end with a NUL character");
    }
    Ok(value.to_owned())
}

#[cfg(test)]
#[path = "../tests/unit/args/tests.rs"]
mod tests;
