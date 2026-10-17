use super::attempt::PaymentAttempt;
#[cfg(test)]
use super::progression::PAYMENT_LOOKUP_TIMEOUT;
use super::submission::OriginalTransactionState;
use super::submission::original_presence_from_lookup;
use crate::db::Activity;
use crate::db::AddressFaucet;
#[cfg(test)]
use crate::db::Store;
#[cfg(test)]
use crate::state::AppState;
use crate::state::notify;
use anyhow::Context;
use serde_json::Value;
#[cfg(test)]
use std::future::Future;
#[cfg(test)]
use tokio::time::Instant;

pub(crate) async fn confirm_address_after_mining_with_attempt(
    attempt: &PaymentAttempt<'_>,
    pending: AddressFaucet,
) -> anyhow::Result<AddressFaucet> {
    let pending = confirm_address_from_chain_with_attempt(attempt, pending).await?;
    if matches!(
        pending.status.as_str(),
        "confirmed" | "failed" | "unconfirmed"
    ) {
        return Ok(pending);
    }
    attempt
        .mine_and_sync(1)
        .await
        .context("automatically mining and synchronizing address faucet")?;
    confirm_address_from_chain_with_attempt(attempt, pending).await
}

pub(crate) async fn confirm_address_from_chain_with_attempt(
    attempt: &PaymentAttempt<'_>,
    pending: AddressFaucet,
) -> anyhow::Result<AddressFaucet> {
    if matches!(pending.status.as_str(), "confirmed" | "failed") {
        return Ok(pending);
    }
    let state = attempt.state;
    let transaction = attempt
        .lookup_transaction(&pending.txid)
        .await
        .context("looking up address faucet confirmation")?;
    let transaction = confirmation_lookup(&pending.status, transaction);
    let updated = match original_presence_from_lookup(attempt, &pending.txid, transaction)
        .await
        .context("checking address faucet confirmation")?
    {
        OriginalTransactionState::Included(hash) => {
            state
                .0
                .store
                .confirm_address(&pending.id, &pending.txid, &hash)?
        }
        OriginalTransactionState::Missing | OriginalTransactionState::Present => state
            .0
            .store
            .clear_address_error(&pending.id, &pending.status, &pending.txid)?,
        OriginalTransactionState::Expired => unreachable!(),
    };
    if updated != pending {
        notify(state, "wallet");
    }
    Ok(updated)
}

pub(crate) fn confirmed_block_hash(tx: &Value) -> Option<&str> {
    let confirmations = tx.get("confirmations").and_then(Value::as_u64).unwrap_or(0);
    if confirmations == 0 {
        return None;
    }
    tx.get("blockhash")
        .and_then(Value::as_str)
        .filter(|hash| !hash.is_empty())
}

fn confirmation_lookup(status: &str, transaction: Option<Value>) -> Option<Value> {
    // Side-chain evidence is expected for an intentionally orphaned transaction;
    // it is not permission to replace or rebroadcast that payment.
    transaction.filter(|tx| {
        status != "unconfirmed"
            || (tx.get("in_active_chain").and_then(Value::as_bool) != Some(false)
                && tx
                    .get("confirmations")
                    .and_then(Value::as_i64)
                    .is_none_or(|count| count >= 0))
    })
}

#[cfg(test)]
pub(crate) fn apply_confirmation(
    store: &Store,
    pending: &Activity,
    tx: &Value,
) -> anyhow::Result<Activity> {
    match confirmed_block_hash(tx) {
        Some(hash) => store.confirm(&pending.id, &pending.txid, hash),
        None => Ok(pending.clone()),
    }
}

pub(crate) async fn confirm_from_chain_with_attempt(
    attempt: &PaymentAttempt<'_>,
    pending: Activity,
) -> anyhow::Result<Activity> {
    if matches!(pending.status.as_str(), "confirmed" | "failed") {
        return Ok(pending);
    }
    let state = attempt.state;
    let transaction = attempt
        .lookup_transaction(&pending.txid)
        .await
        .context("looking up payment confirmation")?;
    let transaction = confirmation_lookup(&pending.status, transaction);
    let updated = match original_presence_from_lookup(attempt, &pending.txid, transaction)
        .await
        .context("checking payment confirmation")?
    {
        OriginalTransactionState::Included(hash) => {
            state.0.store.confirm(&pending.id, &pending.txid, &hash)?
        }
        OriginalTransactionState::Missing | OriginalTransactionState::Present => state
            .0
            .store
            .clear_error(&pending.id, &pending.status, &pending.txid)?,
        OriginalTransactionState::Expired => unreachable!(),
    };
    if updated != pending {
        notify(state, "wallet");
    }
    Ok(updated)
}

#[cfg(test)]
pub(crate) async fn reconcile_unconfirmed_with<F, Fut>(
    store: &Store,
    cursor: &mut Option<String>,
    deadline: Instant,
    mut lookup: F,
) -> anyhow::Result<bool>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = anyhow::Result<Option<Value>>>,
{
    let mut activities = store.unconfirmed_activities()?;
    if activities.is_empty() {
        *cursor = None;
        return Ok(false);
    }
    // Stable ID ordering retains the resume point even if the last attempted
    // record was confirmed and has disappeared from the unresolved set.
    activities.sort_by(|left, right| left.id.cmp(&right.id));
    let start = cursor.as_ref().map_or(0, |id| {
        activities.partition_point(|activity| activity.id.as_str() <= id.as_str())
            % activities.len()
    });
    let mut changed = false;
    for offset in 0..activities.len() {
        if Instant::now() >= deadline {
            break;
        }
        let activity = &activities[(start + offset) % activities.len()];
        // Advance even after an uncertain lookup, so an outage for one record
        // cannot monopolize every subsequent bounded pass.
        *cursor = Some(activity.id.clone());
        let attempt_deadline = deadline.min(Instant::now() + PAYMENT_LOOKUP_TIMEOUT);
        let result: anyhow::Result<bool> = async {
            let transaction = crate::reconcile::within_deadline(
                attempt_deadline,
                "checking payment confirmation",
                lookup(activity.txid.clone()),
            )
            .await?;
            if let Some(transaction) = transaction {
                let updated = apply_confirmation(store, activity, &transaction)?;
                Ok(updated.status == "confirmed")
            } else {
                Ok(false)
            }
        }
        .await;
        match result {
            Ok(confirmed) => changed |= confirmed,
            Err(error) => tracing::warn!(
                error = %format_args!("{error:#}"),
                activity = %activity.id,
                txid = %activity.txid,
                "payment confirmation remains pending"
            ),
        }
    }
    Ok(changed)
}

#[cfg(test)]
pub(crate) async fn confirm_after_mining(
    state: &AppState,
    pending: Activity,
) -> anyhow::Result<Activity> {
    confirm_after_mining_with_attempt(
        &PaymentAttempt {
            state,
            dispatch_deadline: None,
        },
        pending,
    )
    .await
}

pub(crate) async fn confirm_after_mining_with_attempt(
    attempt: &PaymentAttempt<'_>,
    pending: Activity,
) -> anyhow::Result<Activity> {
    let pending = confirm_from_chain_with_attempt(attempt, pending).await?;
    if matches!(
        pending.status.as_str(),
        "confirmed" | "failed" | "unconfirmed"
    ) {
        return Ok(pending);
    }
    attempt
        .mine_and_sync(1)
        .await
        .context("automatically mining and synchronizing payment")?;
    confirm_from_chain_with_attempt(attempt, pending).await
}

#[cfg(test)]
#[path = "../../tests/unit/payments/confirmation/tests.rs"]
pub(crate) mod tests;
