mod runtime;
mod updater;

use std::{path::PathBuf, process::ExitCode, str::FromStr};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use runtime::{InstanceName, Runtime};

#[derive(Parser)]
#[command(
    name = "ths",
    version,
    about = "A one-command Zakura regtest environment"
)]
struct Cli {
    /// Isolated environment name.
    #[arg(long, global = true, default_value = "default")]
    name: InstanceName,
    /// Print machine-readable output where supported.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start a local Regtest environment in the foreground.
    Start {
        #[arg(long)]
        no_open: bool,
        /// Run a locally built Zakura executable instead of the node image.
        #[arg(long, value_name = "PATH", conflicts_with = "zakura_rpc")]
        zakura_bin: Option<PathBuf>,
        /// Attach to a localhost Regtest node using a wallet created by `ths prepare`.
        #[arg(long, value_name = "LOCAL_URL")]
        zakura_rpc: Option<String>,
    },
    /// Prepare a wallet and configuration for a local Regtest node you start yourself.
    Prepare {
        /// HTTP RPC origin on 127.0.0.1 or localhost; internet and LAN nodes are unsupported.
        #[arg(long, value_name = "LOCAL_URL")]
        zakura_rpc: String,
    },
    /// Build the runtime images from the current source.
    Build {
        /// Keep workspace Rust code unoptimized while optimizing dependencies.
        #[arg(long)]
        dev: bool,
        /// Build only the app and lightwalletd images for use with a local node.
        #[arg(long)]
        without_zakura: bool,
    },
    /// Pull the exact runtime images for this launcher version.
    Pull {
        /// Pull only the app and lightwalletd images for use with a local node.
        #[arg(long)]
        without_zakura: bool,
    },
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
    Endpoints,
    /// Mine blocks on a running development environment.
    Mine {
        /// Number of blocks to mine.
        #[arg(value_parser = clap::value_parser!(u32).range(1..=10_000))]
        blocks: u32,
    },
    /// Send disposable Regtest ZEC to a unified or transparent address.
    Faucet {
        /// Regtest unified or transparent destination address.
        address: String,
        /// Amount of ZEC to send (maximum 5, up to 8 decimal places).
        #[arg(long, default_value = "1")]
        amount: ZecAmount,
    },
    /// Stream or print service logs.
    Logs {
        #[arg(value_parser = ["app", "zakura", "lightwalletd"])]
        service: Option<String>,
        #[arg(short, long)]
        follow: bool,
    },
    /// Stop managed nodes and delete their data; detach from self-managed nodes and retain the wallet.
    Stop,
    /// Delete ths-managed data; preserve self-managed node processes, configuration, and chains.
    Reset {
        #[arg(long)]
        force: bool,
    },
    /// List known environments.
    List,
    /// Check local Docker and configuration prerequisites.
    Doctor,
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    if let Some(Command::Update { version, check }) = &cli.command {
        return updater::run(version.as_deref(), *check, cli.json);
    }
    if matches!(cli.command, Some(Command::Uninstall)) {
        return updater::uninstall();
    }
    if should_check_for_updates(&cli)
        && let Some(notice) = updater::startup_notice()
    {
        println!("{notice}");
    }
    let runtime = Runtime::discover()?;
    match cli.command.unwrap_or(Command::Start {
        no_open: false,
        zakura_bin: None,
        zakura_rpc: None,
    }) {
        Command::Start {
            no_open,
            zakura_bin,
            zakura_rpc,
        } => runtime.start(
            &cli.name,
            no_open,
            cli.json,
            zakura_bin.as_deref(),
            zakura_rpc.as_deref(),
        ),
        Command::Prepare { zakura_rpc } => runtime.prepare(&cli.name, &zakura_rpc, cli.json),
        Command::Build {
            dev,
            without_zakura,
        } => runtime.build(dev, without_zakura),
        Command::Pull { without_zakura } => runtime.pull(without_zakura),
        Command::Update { .. } => unreachable!("update is handled before runtime discovery"),
        Command::Uninstall => unreachable!("uninstall is handled before runtime discovery"),
        Command::Status => runtime.status(&cli.name, cli.json),
        Command::Open => runtime.open(&cli.name),
        Command::Endpoints => runtime.endpoints(&cli.name, cli.json),
        Command::Mine { blocks } => runtime.mine(&cli.name, blocks, cli.json),
        Command::Faucet { address, amount } => {
            runtime.faucet(&cli.name, &address, amount.zatoshi(), cli.json)
        }
        Command::Logs { service, follow } => runtime.logs(&cli.name, service.as_deref(), follow),
        Command::Stop => runtime.stop(&cli.name),
        Command::Reset { force } => runtime.reset(&cli.name, force),
        Command::List => runtime.list(cli.json),
        Command::Doctor => runtime.doctor(cli.json),
    }?;
    Ok(ExitCode::SUCCESS)
}

fn should_check_for_updates(cli: &Cli) -> bool {
    !cli.json && matches!(cli.command, None | Some(Command::Start { .. }))
}

fn _assert_pathbuf_send(_: PathBuf) {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ZecAmount(u64);

impl ZecAmount {
    fn zatoshi(self) -> u64 {
        self.0
    }
}

impl FromStr for ZecAmount {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
        if whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.len() > 8
        {
            bail!("amount must be a decimal ZEC value with at most 8 decimal places");
        }
        let whole = whole.parse::<u64>()?;
        let fraction = if fraction.is_empty() {
            0
        } else {
            fraction.parse::<u64>()? * 10u64.pow(8 - fraction.len() as u32)
        };
        let zatoshi = whole
            .checked_mul(100_000_000)
            .and_then(|value| value.checked_add(fraction))
            .ok_or_else(|| anyhow::anyhow!("amount is too large"))?;
        if zatoshi == 0 || zatoshi > 500_000_000 {
            bail!("amount must be greater than zero and no more than 5 ZEC");
        }
        Ok(Self(zatoshi))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_node_workflows_and_rejects_conflicting_sources() {
        let cli =
            Cli::try_parse_from(["ths", "start", "--zakura-bin", "/build with spaces/zakurad"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Start {
                zakura_bin: Some(_),
                zakura_rpc: None,
                ..
            })
        ));
        assert!(
            Cli::try_parse_from([
                "ths",
                "start",
                "--zakura-bin",
                "/node",
                "--zakura-rpc",
                "http://127.0.0.1:18232"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["ths", "prepare"]).is_err());
        let cli = Cli::try_parse_from([
            "ths",
            "prepare",
            "--zakura-rpc",
            "http://127.0.0.1:18232",
            "--name",
            "debug",
        ])
        .unwrap();
        assert!(matches!(cli.command, Some(Command::Prepare { .. })));
        assert_eq!(cli.name.to_string(), "debug");
        assert!(matches!(
            Cli::try_parse_from(["ths", "pull", "--without-zakura"])
                .unwrap()
                .command,
            Some(Command::Pull {
                without_zakura: true
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["ths", "build", "--dev", "--without-zakura"])
                .unwrap()
                .command,
            Some(Command::Build {
                dev: true,
                without_zakura: true
            })
        ));
    }

    #[test]
    fn separates_building_from_starting() {
        let default = Cli::try_parse_from(["ths"]).unwrap();
        assert!(default.command.is_none());

        let cli = Cli::try_parse_from(["ths", "build", "--dev"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Build {
                dev: true,
                without_zakura: false
            })
        ));

        let cli = Cli::try_parse_from(["ths", "pull"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Pull {
                without_zakura: false
            })
        ));

        let cli = Cli::try_parse_from(["ths", "update", "v1.2.3"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Update {
                version: Some(_),
                check: false
            })
        ));

        let cli = Cli::try_parse_from(["ths", "uninstall"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Uninstall)));

        let cli = Cli::try_parse_from(["ths", "mine", "10"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Mine { blocks: 10 })));

        let cli = Cli::try_parse_from(["ths", "--name", "alice", "mine", "3"]).unwrap();
        assert_eq!(cli.name.to_string(), "alice");
        assert!(matches!(cli.command, Some(Command::Mine { blocks: 3 })));

        let cli = Cli::try_parse_from(["ths", "mine", "3", "--name", "alice"]).unwrap();
        assert_eq!(cli.name.to_string(), "alice");

        assert!(Cli::try_parse_from(["ths", "mine", "0"]).is_err());
        assert!(Cli::try_parse_from(["ths", "mine", "10001"]).is_err());

        let cli = Cli::try_parse_from(["ths", "faucet", "uregtest1example"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Faucet {
                amount: ZecAmount(100_000_000),
                ..
            })
        ));

        let cli = Cli::try_parse_from([
            "ths",
            "faucet",
            "tmExample",
            "--amount",
            "1.25",
            "--name",
            "alice",
        ])
        .unwrap();
        assert_eq!(cli.name.to_string(), "alice");
        assert!(matches!(
            cli.command,
            Some(Command::Faucet {
                amount: ZecAmount(125_000_000),
                ..
            })
        ));

        assert!(Cli::try_parse_from(["ths", "update", "1.2.3", "--check"]).is_err());

        assert!(Cli::try_parse_from(["ths", "start", "--build"]).is_err());
    }

    #[test]
    fn checks_for_updates_only_during_human_readable_startup() {
        let default = Cli::try_parse_from(["ths"]).unwrap();
        assert!(should_check_for_updates(&default));

        let start = Cli::try_parse_from(["ths", "start", "--no-open"]).unwrap();
        assert!(should_check_for_updates(&start));

        let json = Cli::try_parse_from(["ths", "--json"]).unwrap();
        assert!(!should_check_for_updates(&json));

        let status = Cli::try_parse_from(["ths", "status"]).unwrap();
        assert!(!should_check_for_updates(&status));
    }

    #[test]
    fn parses_exact_zec_amounts() {
        assert_eq!("0.00000001".parse::<ZecAmount>().unwrap().zatoshi(), 1);
        assert_eq!("5".parse::<ZecAmount>().unwrap().zatoshi(), 500_000_000);
        for invalid in ["0", "5.00000001", "1.000000001", "-1", "1e2", ".5"] {
            assert!(invalid.parse::<ZecAmount>().is_err(), "accepted {invalid}");
        }
    }
}
