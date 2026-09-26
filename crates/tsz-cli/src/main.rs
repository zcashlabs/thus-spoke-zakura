mod runtime;
mod updater;

use std::{ffi::OsString, path::PathBuf, process::ExitCode, str::FromStr, time::Duration};

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
    #[arg(long, global = true)]
    name: Option<InstanceName>,
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

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    if cli.json && matches!(cli.command, Some(Command::Run { .. })) {
        bail!("--json cannot be used with `run`; the child command owns stdout");
    }
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
    let run = matches!(cli.command, Some(Command::Run { .. }));
    let name = resolve_name(cli.name, run);
    let runtime = Runtime::discover()?;
    match cli.command.unwrap_or(Command::Start { no_open: false }) {
        Command::Start { no_open } => runtime.start(&name, no_open, cli.json),
        Command::Wait { timeout } => runtime.wait(&name, Duration::from_secs(timeout), cli.json),
        Command::Run { timeout, command } => {
            return runtime
                .run(&name, Duration::from_secs(timeout), &command)
                .map(ExitCode::from);
        }
        Command::Build { dev } => runtime.build(dev),
        Command::Pull => runtime.pull(),
        Command::Update { .. } => unreachable!("update is handled before runtime discovery"),
        Command::Uninstall => unreachable!("uninstall is handled before runtime discovery"),
        Command::Status => runtime.status(&name, cli.json),
        Command::Open => runtime.open(&name),
        Command::Endpoints => runtime.endpoints(&name, cli.json),
        Command::Mine { blocks } => runtime.mine(&name, blocks, cli.json),
        Command::Faucet { address, amount } => {
            runtime.faucet(&name, &address, amount.zatoshi(), cli.json)
        }
        Command::Logs { service, follow } => runtime.logs(&name, service.as_deref(), follow),
        Command::Stop => runtime.stop(&name),
        Command::Reset { force } => runtime.reset(&name, force),
        Command::List => runtime.list(cli.json),
        Command::Doctor => runtime.doctor(cli.json),
    }?;
    Ok(ExitCode::SUCCESS)
}

fn resolve_name(name: Option<InstanceName>, run: bool) -> InstanceName {
    name.unwrap_or_else(|| {
        if run {
            generated_run_name()
        } else {
            "default".parse().expect("default instance name is valid")
        }
    })
}

fn generated_run_name() -> InstanceName {
    format!("run-{}", uuid::Uuid::new_v4().simple())
        .parse()
        .expect("UUID-based run instance name is valid")
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
        assert_eq!(cli.name.as_ref().unwrap().to_string(), "alice");
        assert!(matches!(cli.command, Some(Command::Mine { blocks: 3 })));

        let cli = Cli::try_parse_from(["ths", "mine", "3", "--name", "alice"]).unwrap();
        assert_eq!(cli.name.as_ref().unwrap().to_string(), "alice");

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
        assert_eq!(cli.name.as_ref().unwrap().to_string(), "alice");
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
    fn parses_wait_and_run_without_reusing_the_default_name() {
        let run = Cli::try_parse_from(["ths", "run", "--", "npm", "test"]).unwrap();
        assert!(run.name.is_none());
        assert!(matches!(
            run.command,
            Some(Command::Run { timeout: 120, command })
                if command == [OsString::from("npm"), OsString::from("test")]
        ));

        let wait = Cli::try_parse_from(["ths", "--name", "api", "wait", "--timeout", "7"]).unwrap();
        assert_eq!(wait.name.as_ref().unwrap().to_string(), "api");
        assert!(matches!(wait.command, Some(Command::Wait { timeout: 7 })));

        assert!(Cli::try_parse_from(["ths", "--json", "run", "--", "true"]).is_ok());

        let first = generated_run_name();
        let second = generated_run_name();
        assert_ne!(first.to_string(), second.to_string());
        assert!(first.to_string().starts_with("run-"));

        assert_eq!(resolve_name(None, false).to_string(), "default");
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
