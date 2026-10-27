use anyhow::{Result, bail};

use crate::{
    args::{Pool, SendAmount, SendArgs},
    client::TransferAmount,
    runtime::{InstanceName, Runtime},
};

/// Regtest unified addresses start with `u` and transparent ones with `t`. The
/// server validates the address itself and rejects one that cannot receive into
/// the chosen pool, so only the pool is inferred here.
fn address_pool(address: &str) -> Pool {
    if address.starts_with('t') {
        Pool::Transparent
    } else {
        Pool::Ironwood
    }
}

fn check_send(args: &SendArgs) -> Result<()> {
    if args.to == Some(args.from) && args.source_pool == args.destination_pool {
        bail!("choose a different --to account or a different --destination-pool");
    }
    if args.memo.is_some() && args.destination_pool == Pool::Transparent {
        if args.to_address.is_some() {
            bail!(
                "--memo requires an address that can receive Ironwood; transparent outputs cannot carry a memo"
            );
        }
        bail!(
            "--memo requires --destination-pool ironwood; transparent outputs cannot carry a memo"
        );
    }
    Ok(())
}

pub(super) fn send(
    runtime: &Runtime,
    name: &InstanceName,
    mut args: SendArgs,
    json: bool,
) -> Result<()> {
    if let Some(address) = &args.to_address {
        args.destination_pool = address_pool(address);
    }
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
    if let Some(address) = &args.to_address {
        let Some(amount) = amount else {
            bail!("--to-address requires either --amount or --max");
        };
        return runtime.wallet_send_address(
            name,
            args.from,
            address,
            args.source_pool.as_str(),
            args.destination_pool.as_str(),
            amount,
            args.memo.as_deref(),
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
