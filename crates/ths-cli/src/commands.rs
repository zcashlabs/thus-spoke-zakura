mod wallet;

use std::{io::Write, process::ExitCode, time::Duration};

use clap::CommandFactory;

use crate::{
    args::{Cli, Command, CompletionShell, Pool, SendArgs, WalletCommand},
    runtime::{InstanceName, MiningConfig, Runtime},
    updater,
};
use wallet::send;

pub(crate) fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    if cli.json && matches!(cli.command, Some(Command::Run { .. })) {
        anyhow::bail!("--json cannot be used with `run`; the child command owns stdout");
    }
    if cli.json && matches!(cli.command, Some(Command::Endpoints { env: true })) {
        anyhow::bail!("--json cannot be used with `endpoints --env`");
    }
    if let Some(Command::Update { version, check }) = &cli.command {
        return updater::run(version.as_deref(), *check, cli.json);
    }
    if matches!(cli.command, Some(Command::Uninstall)) {
        return updater::uninstall();
    }
    // The script is the entire stdout. Runtime discovery and the startup
    // update notice must not run first, or they would corrupt it.
    if let Some(Command::Completions { shell }) = cli.command {
        return print_completions(shell);
    }
    if should_check_for_updates(&cli)
        && let Some(notice) = updater::startup_notice()
    {
        println!("{notice}");
    }
    let run = matches!(cli.command, Some(Command::Run { .. }));
    let name = resolve_name(cli.name, run);
    let runtime = Runtime::discover()?;
    match cli.command.unwrap_or(Command::Start {
        no_open: false,
        detach: false,
        port_offset: 0,
        block_time: None,
        mine_on_mempool: false,
        no_auto_mine: false,
    }) {
        Command::Start {
            no_open,
            detach,
            port_offset,
            block_time,
            mine_on_mempool,
            no_auto_mine,
        } => {
            let mining = MiningConfig::from_flags(block_time, mine_on_mempool, no_auto_mine);
            runtime.start(&name, no_open, detach, cli.json, port_offset, mining)
        }
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
        Command::Status => {
            return runtime
                .status(&name, cli.json)
                .map(|running| ExitCode::from(u8::from(!running)));
        }
        Command::Open => runtime.open(&name),
        Command::Endpoints { env } => runtime.endpoints(&name, cli.json, env),
        Command::Rpc {
            raw,
            yes,
            method,
            params,
        } => runtime.rpc(&name, &method, &params, raw, yes),
        Command::Tx { txid } => runtime.explorer(&name, "transactions", Some(&txid), cli.json),
        Command::Block { id } => runtime.explorer(&name, "blocks", id.as_deref(), cli.json),
        Command::Mine { blocks } => runtime.mine(&name, blocks, cli.json),
        Command::Reorg { depth, keep_txs } => {
            runtime.change_chain(&name, "reorg", depth, keep_txs, cli.json)
        }
        Command::Rollback { depth } => {
            runtime.change_chain(&name, "rollback", depth, false, cli.json)
        }
        Command::Faucet { address, amount } => {
            runtime.faucet(&name, &address, amount.zatoshi(), cli.json)
        }
        Command::Wallet { action } => match action {
            WalletCommand::Balances => runtime.wallet_balances(&name, cli.json),
            WalletCommand::Activity { limit } => runtime.wallet_activity(&name, limit, cli.json),
            WalletCommand::Quote {
                from,
                source_pool,
                destination_pool,
            } => runtime.wallet_quote(
                &name,
                from,
                source_pool.as_str(),
                destination_pool.as_str(),
                cli.json,
            ),
            WalletCommand::Faucet {
                accounts,
                amount,
                pool,
            } => runtime.wallet_faucet(&name, &accounts, amount.zatoshi(), pool.as_str(), cli.json),
            WalletCommand::Send(args) => send(&runtime, &name, args, cli.json),
            WalletCommand::Shield {
                from,
                to,
                amount,
                max,
                memo,
            } => send(
                &runtime,
                &name,
                SendArgs {
                    from,
                    to: Some(to),
                    to_address: None,
                    amount,
                    max,
                    uri: None,
                    source_pool: Pool::Transparent,
                    destination_pool: Pool::Ironwood,
                    memo,
                },
                cli.json,
            ),
            WalletCommand::Unshield {
                from,
                to,
                amount,
                max,
            } => send(
                &runtime,
                &name,
                SendArgs {
                    from,
                    to: Some(to),
                    to_address: None,
                    amount,
                    max,
                    uri: None,
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
        } => runtime.logs(&name, service.as_deref(), follow, tail, head),
        Command::Stop => runtime.stop(&name),
        Command::Reset { force } => runtime.reset(&name, force),
        Command::List => runtime.list(cli.json),
        Command::Doctor => runtime.doctor(cli.json),
        Command::Completions { .. } => {
            unreachable!("completions are printed before runtime discovery")
        }
    }?;
    Ok(ExitCode::SUCCESS)
}

fn print_completions(shell: CompletionShell) -> anyhow::Result<ExitCode> {
    let mut script = Vec::new();
    write_completions(shell, &mut script);
    let mut stdout = std::io::stdout().lock();
    // A preview such as `ths completions zsh | head` closes stdout early.
    // clap_complete treats that write error as fatal; a completion script is
    // not a failed command when the reader stops.
    if let Err(error) = stdout.write_all(&script).and_then(|()| stdout.flush())
        && error.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(error.into());
    }
    Ok(ExitCode::SUCCESS)
}

fn write_completions(shell: CompletionShell, buf: &mut dyn Write) {
    let mut command = Cli::command();
    clap_complete::generate(shell.generator(), &mut command, "ths", buf);
}

fn should_check_for_updates(cli: &Cli) -> bool {
    !cli.json && matches!(cli.command, None | Some(Command::Start { .. }))
}

#[cfg(test)]
#[path = "../tests/unit/commands/tests.rs"]
mod tests;

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
