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
    /// Start an environment in the foreground; interrupting deletes it.
    Start {
        #[arg(long)]
        no_open: bool,
        /// Add this many to the default loopback ports. Must be a multiple of 10.
        #[arg(long, default_value_t = 0)]
        port_offset: u16,
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
}

#[derive(Subcommand)]
enum WalletCommand {
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
        #[arg(long)]
        amount: SendAmount,
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
        #[arg(long)]
        amount: SendAmount,
    },
}

#[derive(clap::Args)]
struct SendArgs {
    /// Source account index (1-5).
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=5))]
    from: u8,
    /// Destination account index (1-5); may match --from when pools differ.
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=5))]
    to: u8,
    /// Amount of ZEC to send, up to 8 decimal places.
    #[arg(long)]
    amount: SendAmount,
    /// Pool to spend from.
    #[arg(long = "source-pool", value_enum, default_value = "ironwood")]
    source_pool: Pool,
    /// Pool the destination account receives into.
    #[arg(long = "destination-pool", value_enum, default_value = "ironwood")]
    destination_pool: Pool,
    /// Text memo for the recipient (up to 512 bytes; ironwood destinations only).
    #[arg(long, value_parser = parse_memo)]
    memo: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
enum Pool {
    Ironwood,
    Transparent,
}

impl Pool {
    fn as_str(self) -> &'static str {
        match self {
            Pool::Ironwood => "ironwood",
            Pool::Transparent => "transparent",
        }
    }
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
        port_offset: 0,
    }) {
        Command::Start {
            no_open,
            port_offset,
        } => runtime.start(&cli.name, no_open, cli.json, port_offset),
        Command::Build { dev } => runtime.build(dev),
        Command::Pull => runtime.pull(),
        Command::Update { .. } => unreachable!("update is handled before runtime discovery"),
        Command::Uninstall => unreachable!("uninstall is handled before runtime discovery"),
        Command::Status => runtime.status(&cli.name, cli.json),
        Command::Open => runtime.open(&cli.name),
        Command::Endpoints => runtime.endpoints(&cli.name, cli.json),
        Command::Mine { blocks } => runtime.mine(&cli.name, blocks, cli.json),
        Command::Faucet { address, amount } => {
            runtime.faucet(&cli.name, &address, amount.zatoshi(), cli.json)
        }
        Command::Wallet { action } => match action {
            WalletCommand::Faucet {
                accounts,
                amount,
                pool,
            } => runtime.wallet_faucet(
                &cli.name,
                &accounts,
                amount.zatoshi(),
                pool.as_str(),
                cli.json,
            ),
            WalletCommand::Send(args) => send(&runtime, &cli.name, args, cli.json),
            WalletCommand::Shield {
                from,
                to,
                amount,
                memo,
            } => send(
                &runtime,
                &cli.name,
                SendArgs {
                    from,
                    to,
                    amount,
                    source_pool: Pool::Transparent,
                    destination_pool: Pool::Ironwood,
                    memo,
                },
                cli.json,
            ),
            WalletCommand::Unshield { from, to, amount } => send(
                &runtime,
                &cli.name,
                SendArgs {
                    from,
                    to,
                    amount,
                    source_pool: Pool::Ironwood,
                    destination_pool: Pool::Transparent,
                    memo: None,
                },
                cli.json,
            ),
        },
        Command::Logs {
            service,
            follow,
            tail,
            head,
        } => runtime.logs(&cli.name, service.as_deref(), follow, tail, head),
        Command::Stop => runtime.stop(&cli.name),
        Command::Reset { force } => runtime.reset(&cli.name, force),
        Command::List => runtime.list(cli.json),
        Command::Doctor => runtime.doctor(cli.json),
    }?;
    Ok(ExitCode::SUCCESS)
}

fn check_send(args: &SendArgs) -> Result<()> {
    if args.from == args.to && args.source_pool == args.destination_pool {
        bail!("choose a different --to account or a different --destination-pool");
    }
    if args.memo.is_some() && args.destination_pool == Pool::Transparent {
        bail!(
            "--memo requires --destination-pool ironwood; transparent outputs cannot carry a memo"
        );
    }
    Ok(())
}

fn send(runtime: &Runtime, name: &InstanceName, args: SendArgs, json: bool) -> Result<()> {
    check_send(&args)?;
    runtime.wallet_send(
        name,
        args.from,
        args.to,
        args.source_pool.as_str(),
        args.destination_pool.as_str(),
        args.amount.zatoshi(),
        args.memo.as_deref(),
        json,
    )
}

fn should_check_for_updates(cli: &Cli) -> bool {
    !cli.json && matches!(cli.command, None | Some(Command::Start { .. }))
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

fn parse_zec_zatoshi(value: &str) -> Result<u64> {
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
    whole
        .checked_mul(100_000_000)
        .and_then(|value| value.checked_add(fraction))
        .ok_or_else(|| anyhow::anyhow!("amount is too large"))
}

/// A ZEC amount limited to 5, matching the server's per-request faucet cap.
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
        let zatoshi = parse_zec_zatoshi(value)?;
        if zatoshi == 0 || zatoshi > 500_000_000 {
            bail!("amount must be greater than zero and no more than 5 ZEC");
        }
        Ok(Self(zatoshi))
    }
}

/// A ZEC amount with no upper bound, for transfers between accounts that are
/// only limited by the sending account's balance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SendAmount(u64);

impl SendAmount {
    fn zatoshi(self) -> u64 {
        self.0
    }
}

impl FromStr for SendAmount {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let zatoshi = parse_zec_zatoshi(value)?;
        if zatoshi == 0 {
            bail!("amount must be greater than zero");
        }
        Ok(Self(zatoshi))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separates_building_from_starting() {
        let default = Cli::try_parse_from(["ths"]).unwrap();
        assert!(default.command.is_none());

        let cli = Cli::try_parse_from(["ths", "build", "--dev"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Build { dev: true })));

        let cli = Cli::try_parse_from(["ths", "pull"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Pull)));

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

        let cli = Cli::try_parse_from(["ths", "start", "--port-offset", "10"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Start {
                no_open: false,
                port_offset: 10
            })
        ));

        assert!(Cli::try_parse_from(["ths", "start", "--port-offset", "1"]).is_ok());
    }

    #[test]
    fn parses_log_line_selection() {
        let cli = Cli::try_parse_from(["ths", "logs"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Logs {
                service: None,
                follow: false,
                tail: None,
                head: None,
            })
        ));

        let cli = Cli::try_parse_from(["ths", "logs", "zakura", "--tail", "200", "-f"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Logs {
                service: Some(_),
                follow: true,
                tail: Some(200),
                head: None,
            })
        ));

        let cli = Cli::try_parse_from(["ths", "logs", "lightwalletd", "--head", "200"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Logs {
                service: Some(_),
                follow: false,
                tail: None,
                head: Some(200),
            })
        ));

        for invalid in [
            &["ths", "logs", "--tail", "0"][..],
            &["ths", "logs", "--head", "0"],
            &["ths", "logs", "--tail", "-1"],
            &["ths", "logs", "--head", "1.5"],
            &["ths", "logs", "--tail", "many"],
            &["ths", "logs", "--head", "5", "--tail", "5"],
            &["ths", "logs", "--head", "5", "--follow"],
        ] {
            assert!(
                Cli::try_parse_from(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
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
    fn wallet_send_and_shield_accept_bounded_memos() {
        let cli = Cli::try_parse_from([
            "ths",
            "wallet",
            "send",
            "--from",
            "1",
            "--to",
            "2",
            "--amount",
            "1.5",
            "--source-pool",
            "transparent",
            "--memo",
            "hello",
        ])
        .unwrap();
        let Some(Command::Wallet {
            action: WalletCommand::Send(args),
        }) = cli.command
        else {
            panic!("expected ths wallet send");
        };
        assert_eq!((args.from, args.to), (1, 2));
        assert_eq!(args.amount.zatoshi(), 150_000_000);
        assert_eq!(args.source_pool, Pool::Transparent);
        assert_eq!(args.destination_pool, Pool::Ironwood);
        assert_eq!(args.memo.as_deref(), Some("hello"));

        assert!(
            Cli::try_parse_from([
                "ths", "wallet", "send", "--from", "1", "--to", "6", "--amount", "1"
            ])
            .is_err()
        );

        // Account operations live only under `ths wallet`.
        for removed in ["send", "deploy"] {
            assert!(
                Cli::try_parse_from(["ths", removed, "--from", "1", "--to", "2", "--amount", "1"])
                    .is_err(),
                "ths {removed} should not exist"
            );
        }

        let cli = Cli::try_parse_from([
            "ths", "wallet", "shield", "--from", "1", "--to", "2", "--amount", "1", "--memo", "hi",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Wallet {
                action: WalletCommand::Shield {
                    to: 2,
                    memo: Some(_),
                    ..
                }
            })
        ));

        let too_long = "a".repeat(513);
        assert!(
            Cli::try_parse_from([
                "ths", "wallet", "send", "--from", "1", "--to", "2", "--amount", "1", "--memo",
                &too_long,
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "ths", "wallet", "unshield", "--from", "1", "--to", "2", "--amount", "1", "--memo",
                "x",
            ])
            .is_err()
        );
        // Shield and unshield no longer default --to to --from.
        assert!(
            Cli::try_parse_from(["ths", "wallet", "shield", "--from", "1", "--amount", "1"])
                .is_err()
        );
    }

    fn send_args(from: u8, to: u8, destination_pool: Pool, memo: Option<&str>) -> SendArgs {
        SendArgs {
            from,
            to,
            amount: SendAmount(1),
            source_pool: Pool::Ironwood,
            destination_pool,
            memo: memo.map(str::to_owned),
        }
    }

    #[test]
    fn sends_allow_the_same_account_only_between_pools() {
        for to in [1, 2] {
            for source in [Pool::Ironwood, Pool::Transparent] {
                for destination in [Pool::Ironwood, Pool::Transparent] {
                    let mut args = send_args(1, to, destination, None);
                    args.source_pool = source;
                    assert_eq!(check_send(&args).is_ok(), to != 1 || source != destination);
                }
            }
        }
        let mut shield = send_args(1, 1, Pool::Ironwood, Some("memo"));
        shield.source_pool = Pool::Transparent;
        assert!(check_send(&shield).is_ok());
        assert!(check_send(&send_args(1, 1, Pool::Transparent, Some("hi"))).is_err());
        assert!(check_send(&send_args(1, 1, Pool::Transparent, Some(""))).is_err());
    }

    #[test]
    fn memo_values_follow_the_server_contract() {
        assert_eq!(parse_memo("").unwrap(), "");
        assert!(parse_memo(&"a".repeat(512)).is_ok());
        assert!(parse_memo(&"a".repeat(513)).is_err());
        assert!(parse_memo(&"🌸".repeat(128)).is_ok());
        assert!(parse_memo(&"🌸".repeat(129)).is_err());
        assert!(parse_memo("hi\0").is_err());
        assert!(parse_memo("a\0b").is_ok());
    }

    #[test]
    fn sends_are_checked_before_contacting_the_environment() {
        assert!(check_send(&send_args(1, 2, Pool::Ironwood, Some(""))).is_ok());
        assert!(check_send(&send_args(1, 2, Pool::Transparent, None)).is_ok());
        assert!(check_send(&send_args(3, 3, Pool::Ironwood, None)).is_err());
        assert!(check_send(&send_args(1, 2, Pool::Transparent, Some("hi"))).is_err());
        assert!(check_send(&send_args(1, 2, Pool::Transparent, Some(""))).is_err());
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
