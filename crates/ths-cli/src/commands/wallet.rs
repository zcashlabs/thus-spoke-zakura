use anyhow::{Result, bail};

use crate::{
    args::{Pool, SendAmount, SendArgs},
    client::TransferAmount,
    runtime::{InstanceName, Runtime},
};

fn check_send(args: &SendArgs) -> Result<()> {
    if args.to == Some(args.from) && args.source_pool == args.destination_pool {
        bail!("choose a different --to account or a different --destination-pool");
    }
    if args.memo.is_some() && args.destination_pool == Pool::Transparent {
        bail!(
            "--memo requires --destination-pool ironwood; transparent outputs cannot carry a memo"
        );
    }
    Ok(())
}

pub(super) fn send(
    runtime: &Runtime,
    name: &InstanceName,
    args: SendArgs,
    json: bool,
) -> Result<()> {
    check_send(&args)?;
    let amount = if args.max {
        Some(TransferAmount::Max)
    } else {
        args.amount
            .map(SendAmount::zatoshi)
            .map(TransferAmount::Exact)
    };
    if let Some(uri) = &args.uri {
        return runtime.wallet_send_uri(
            name,
            args.from,
            args.source_pool.as_str(),
            uri,
            amount,
            json,
        );
    }
    let (Some(to), Some(amount)) = (args.to, amount) else {
        bail!("--to and either --amount or --max are required unless --uri is given");
    };
    runtime.wallet_send(
        name,
        args.from,
        to,
        args.source_pool.as_str(),
        args.destination_pool.as_str(),
        amount,
        args.memo.as_deref(),
        json,
    )
}

#[cfg(test)]
#[path = "../../tests/unit/commands/wallet/tests.rs"]
mod tests;
