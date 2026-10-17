use super::attempt::PaymentAttempt;
use super::confirmation::confirm_address_after_mining_with_attempt;
use super::confirmation::confirm_from_chain_with_attempt;
use super::execution::execute_payment;
use super::execution::parse_payment_memo;
use super::execution::record_prepared_payment;
use super::submission::AddressSubmission;
use super::submission::OriginalTransactionState;
use super::submission::original_transaction_state;
use super::submission::submit_address_prepared;
use super::treasury::prepare_with_replenishment;
use super::treasury::record_prepared_address_payment;
use crate::db::Activity;
use crate::db::AddressFaucet;
use crate::db::Store;
use crate::db::TREASURY_ACCOUNT_ID;
use crate::state::AppState;
use crate::state::notify;
use crate::wallet::PaymentError;
use crate::wallet::PreparedPayment;
use anyhow::Context;
use std::time::Duration;
use tokio::time::Instant;

pub(crate) const PAYMENT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) const PAYMENT_DISPATCH_BUDGET: Duration = Duration::from_secs(10);

pub(crate) const PAYMENT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Default)]
pub(crate) enum PaymentCollection {
    #[default]
    Activity,
    Address,
}

#[derive(Default)]
pub(crate) struct PaymentTraversal {
    pub(crate) activity_cursor: Option<String>,
    pub(crate) address_cursor: Option<String>,
    pub(crate) next_collection: PaymentCollection,
}

pub(crate) enum ActivityRecovery {
    Intent(crate::db::ActivityIntent),
    Legacy(Activity),
}

impl ActivityRecovery {
    pub(crate) fn id(&self) -> &str {
        match self {
            Self::Intent(intent) => &intent.activity.id,
            Self::Legacy(activity) => &activity.id,
        }
    }
}

pub(crate) async fn payment_confirmation_loop(state: AppState) {
    let mut traversal = PaymentTraversal::default();
    let mut interval = tokio::time::interval(PAYMENT_RETRY_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let deadline = Instant::now() + PAYMENT_DISPATCH_BUDGET;
        if state.started()
            && let Err(error) =
                progress_payment_intents_with_traversal(&state, deadline, &mut traversal).await
        {
            tracing::warn!(
                error = %format_args!("{error:#}"),
                "background payment progression failed"
            );
        }
    }
}

#[cfg(test)]
pub(crate) async fn progress_payment_intents(
    state: &AppState,
    deadline: Instant,
    activity_cursor: &mut Option<String>,
    address_cursor: &mut Option<String>,
) -> anyhow::Result<()> {
    let mut traversal = PaymentTraversal {
        activity_cursor: activity_cursor.take(),
        address_cursor: address_cursor.take(),
        next_collection: PaymentCollection::Activity,
    };
    let result = progress_payment_intents_with_traversal(state, deadline, &mut traversal).await;
    *activity_cursor = traversal.activity_cursor;
    *address_cursor = traversal.address_cursor;
    result
}

pub(crate) async fn progress_payment_intents_with_traversal(
    state: &AppState,
    deadline: Instant,
    traversal: &mut PaymentTraversal,
) -> anyhow::Result<()> {
    let attempt = PaymentAttempt {
        state,
        dispatch_deadline: Some(deadline),
    };
    let mut activities: Vec<_> = state
        .0
        .store
        .unresolved_activities()?
        .into_iter()
        .map(ActivityRecovery::Intent)
        .collect();
    activities.extend(
        state
            .0
            .store
            .unconfirmed_legacy_activities()?
            .into_iter()
            .map(ActivityRecovery::Legacy),
    );
    activities.sort_by(|left, right| left.id().cmp(right.id()));
    let mut address_faucets = state.0.store.unresolved_address_faucets()?;
    address_faucets.sort_by(|left, right| left.payment.id.cmp(&right.payment.id));
    let activity_start = traversal.activity_cursor.as_ref().map_or(0, |id| {
        activities.partition_point(|activity| activity.id() <= id.as_str())
            % activities.len().max(1)
    });
    let address_start = traversal.address_cursor.as_ref().map_or(0, |id| {
        address_faucets.partition_point(|intent| intent.payment.id.as_str() <= id.as_str())
            % address_faucets.len().max(1)
    });
    let mut activity_offset = 0;
    let mut address_offset = 0;
    while activity_offset < activities.len() || address_offset < address_faucets.len() {
        if Instant::now() >= deadline {
            break;
        }
        let collection = match traversal.next_collection {
            PaymentCollection::Activity if activity_offset < activities.len() => {
                PaymentCollection::Activity
            }
            PaymentCollection::Address if address_offset < address_faucets.len() => {
                PaymentCollection::Address
            }
            _ if activity_offset < activities.len() => PaymentCollection::Activity,
            _ => PaymentCollection::Address,
        };
        match collection {
            PaymentCollection::Activity => {
                let activity = &activities[(activity_start + activity_offset) % activities.len()];
                activity_offset += 1;
                // Persist the resume point before lock or execution waits, including
                // an attempt that uses the rest of this pass's dispatch budget.
                traversal.activity_cursor = Some(activity.id().to_owned());
                traversal.next_collection = PaymentCollection::Address;
                let mut observed = match activity {
                    ActivityRecovery::Intent(intent) => intent.activity.clone(),
                    ActivityRecovery::Legacy(legacy) => legacy.clone(),
                };
                let result: anyhow::Result<()> = async {
                    let _payment = attempt.lock(&state.0.payments).await?;
                    attempt.check_can_start()?;
                    match activity {
                        ActivityRecovery::Intent(intent) => {
                            if let Some(current) = state.0.store.activity_for_key(&intent.key)?
                                && !matches!(current.status.as_str(), "confirmed" | "failed")
                            {
                                observed = current.clone();
                                progress_activity_until(state, intent, current, deadline).await?;
                            }
                        }
                        ActivityRecovery::Legacy(legacy) => {
                            if let Some(current) =
                                state.0.store.unconfirmed_legacy_activity(&legacy.id)?
                            {
                                observed = current.clone();
                                confirm_from_chain_with_attempt(&attempt, current).await?;
                            }
                        }
                    }
                    Ok(())
                }
                .await;
                if let Err(error) = result {
                    let error_message = format!("{error:#}");
                    let persistence = match activity {
                        ActivityRecovery::Intent(intent) => record_current_activity_error(
                            &state.0.store,
                            &intent.key,
                            &error_message,
                        ),
                        ActivityRecovery::Legacy(legacy) => record_current_legacy_activity_error(
                            &state.0.store,
                            &legacy.id,
                            &error_message,
                        ),
                    };
                    if let Err(persistence_error) = persistence {
                        tracing::warn!(
                            error = %format_args!("{persistence_error:#}"),
                            execution_error = %error_message,
                            activity = %activity.id(),
                            "could not record pending payment error"
                        );
                    }
                    tracing::warn!(
                        error = %error_message,
                        activity = %activity.id(),
                        "accepted payment remains pending"
                    );
                }
                if let Err(error) = notify_activity_change(state, &observed) {
                    tracing::warn!(
                        error = %format_args!("{error:#}"),
                        activity = %activity.id(),
                        "could not reload payment after progression"
                    );
                }
            }
            PaymentCollection::Address => {
                let intent =
                    &address_faucets[(address_start + address_offset) % address_faucets.len()];
                address_offset += 1;
                traversal.address_cursor = Some(intent.payment.id.clone());
                traversal.next_collection = PaymentCollection::Activity;
                let mut observed = intent.payment.clone();
                let result: anyhow::Result<()> = async {
                    let _payment = attempt.lock(&state.0.payments).await?;
                    attempt.check_can_start()?;
                    if let Some(current) = state.0.store.address_faucet_for_key(&intent.key)?
                        && !matches!(current.status.as_str(), "confirmed" | "failed")
                    {
                        observed = current.clone();
                        progress_address_faucet(state, current, deadline).await?;
                    }
                    Ok(())
                }
                .await;
                if let Err(error) = result {
                    let error_message = format!("{error:#}");
                    if let Err(persistence_error) =
                        record_current_address_error(&state.0.store, &intent.key, &error_message)
                    {
                        tracing::warn!(
                            error = %format_args!("{persistence_error:#}"),
                            execution_error = %error_message,
                            payment = %intent.payment.id,
                            "could not record pending address faucet error"
                        );
                    }
                    tracing::warn!(
                        error = %error_message,
                        payment = %intent.payment.id,
                        "accepted address faucet remains pending"
                    );
                }
                if let Err(error) = notify_address_change(state, &intent.key, &observed) {
                    tracing::warn!(
                        error = %format_args!("{error:#}"),
                        payment = %intent.payment.id,
                        "could not reload address faucet after progression"
                    );
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn notify_activity_change(state: &AppState, previous: &Activity) -> anyhow::Result<()> {
    let current = state.0.store.activity(&previous.id)?;
    if current != *previous {
        notify(state, "wallet");
    }
    Ok(())
}

pub(crate) fn notify_address_change(
    state: &AppState,
    key: &str,
    previous: &AddressFaucet,
) -> anyhow::Result<()> {
    let current = state.0.store.address_faucet_for_key(key)?;
    if current.as_ref() != Some(previous) {
        notify(state, "wallet");
    }
    Ok(())
}

pub(crate) fn record_current_legacy_activity_error(
    store: &Store,
    id: &str,
    error: &str,
) -> anyhow::Result<()> {
    if let Some(current) = store.unconfirmed_legacy_activity(id)? {
        store.record_error(&current.id, &current.status, &current.txid, error)?;
    }
    Ok(())
}

pub(crate) fn record_current_activity_error(
    store: &Store,
    key: &str,
    error: &str,
) -> anyhow::Result<()> {
    if let Some(current) = store.activity_for_key(key)? {
        store.record_error(&current.id, &current.status, &current.txid, error)?;
    }
    Ok(())
}

pub(crate) fn record_current_address_error(
    store: &Store,
    key: &str,
    error: &str,
) -> anyhow::Result<()> {
    if let Some(current) = store.address_faucet_for_key(key)? {
        store.record_address_error(&current.id, &current.status, &current.txid, error)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) async fn progress_activity(
    state: &AppState,
    intent: &crate::db::ActivityIntent,
    activity: Activity,
) -> anyhow::Result<()> {
    progress_activity_until(
        state,
        intent,
        activity,
        Instant::now() + PAYMENT_DISPATCH_BUDGET,
    )
    .await
}

pub(crate) async fn progress_activity_until(
    state: &AppState,
    intent: &crate::db::ActivityIntent,
    activity: Activity,
    deadline: Instant,
) -> anyhow::Result<()> {
    let attempt = PaymentAttempt {
        state,
        dispatch_deadline: Some(deadline),
    };
    execute_activity_intent(&attempt, intent, activity).await?;
    Ok(())
}

pub(crate) async fn execute_activity_intent(
    attempt: &PaymentAttempt<'_>,
    intent: &crate::db::ActivityIntent,
    activity: Activity,
) -> anyhow::Result<Activity> {
    execute_payment(attempt, activity, |_, replacement_txid| async move {
        prepare_activity_intent(attempt, intent, replacement_txid.as_deref()).await
    })
    .await
}

pub(crate) fn is_insufficient_funds(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<PaymentError>(),
        Some(PaymentError::InsufficientFunds { .. })
    )
}

#[cfg(test)]
pub(crate) async fn can_fail_insufficient_send(
    state: &AppState,
    activity_id: &str,
) -> anyhow::Result<bool> {
    can_fail_insufficient_send_with_attempt(
        &PaymentAttempt {
            state,
            dispatch_deadline: None,
        },
        activity_id,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn can_fail_insufficient_send_with_attempt(
    attempt: &PaymentAttempt<'_>,
    activity_id: &str,
) -> anyhow::Result<bool> {
    can_fail_insufficient_send_after_preparation(attempt, activity_id, None).await
}

pub(crate) async fn can_fail_insufficient_send_after_preparation(
    attempt: &PaymentAttempt<'_>,
    activity_id: &str,
    replacement_txid: Option<&str>,
) -> anyhow::Result<bool> {
    let state = attempt.state;
    // Preparation has completed while the caller still owns the payment mutex.
    // Local reads may finish beyond the dispatch budget; new RPC waits may not.
    let wallet_prepared = state.0.wallet.prepared(activity_id).await?;
    let current = state.0.store.activity(activity_id)?;
    let server_prepared = state.0.store.optional_prepared_transaction(activity_id)?;
    if current.kind != "send" || matches!(current.status.as_str(), "confirmed" | "failed") {
        return Ok(false);
    }
    if let (Some(wallet), Some(server)) = (&wallet_prepared, &server_prepared) {
        anyhow::ensure!(
            wallet.raw_transaction == server.raw_transaction
                && wallet.expiry_height == server.expiry_height
                && (current.txid.is_empty() || current.txid == wallet.txid),
            "payment {activity_id} has inconsistent wallet and server preparations"
        );
    }
    if let Some(wallet) = &wallet_prepared {
        anyhow::ensure!(
            current.txid.is_empty() || current.txid == wallet.txid,
            "payment {activity_id} wallet journal disagrees with its current transaction"
        );
    }
    if current.status != "preparing" || !current.txid.is_empty() {
        return Ok(false);
    }
    let Some(prepared) = wallet_prepared else {
        anyhow::ensure!(
            server_prepared.is_none(),
            "payment {activity_id} has a server preparation without a wallet journal"
        );
        anyhow::ensure!(
            replacement_txid.is_none(),
            "payment {activity_id} lost its authorized original wallet journal"
        );
        return Ok(true);
    };
    anyhow::ensure!(
        !prepared.txid.is_empty(),
        "payment {activity_id} has an empty wallet preparation txid"
    );
    match original_transaction_state(attempt, &prepared.txid, prepared.expiry_height).await? {
        OriginalTransactionState::Included(hash) => {
            let repaired = state.0.store.record_prepared(
                activity_id,
                &prepared.txid,
                &prepared.raw_transaction,
                prepared.expiry_height,
            )?;
            state.0.store.confirm(&repaired.id, &prepared.txid, &hash)?;
            return Ok(false);
        }
        OriginalTransactionState::Expired => {}
        OriginalTransactionState::Missing | OriginalTransactionState::Present => return Ok(false),
    }
    anyhow::ensure!(
        server_prepared.is_none(),
        "payment {activity_id} is preparing with an unexpected server preparation"
    );
    let Some(replacement_txid) = replacement_txid else {
        return Ok(false);
    };
    anyhow::ensure!(
        replacement_txid == prepared.txid,
        "payment {activity_id} wallet journal changed from its authorized original"
    );

    // Reprove nonpayment above, then reread both stores so a committed replacement
    // or changed operation cannot inherit the original's terminal-failure decision.
    let latest_wallet = state.0.wallet.prepared(activity_id).await?;
    let latest = state.0.store.activity(activity_id)?;
    let latest_server = state.0.store.optional_prepared_transaction(activity_id)?;
    if latest.kind != "send" || latest.status != "preparing" || !latest.txid.is_empty() {
        return Ok(false);
    }
    anyhow::ensure!(
        latest_server.is_none(),
        "payment {activity_id} acquired a server preparation after original reconciliation"
    );
    let latest_wallet = latest_wallet
        .with_context(|| format!("payment {activity_id} lost its original wallet journal"))?;
    anyhow::ensure!(
        latest_wallet.txid == prepared.txid
            && latest_wallet.raw_transaction == prepared.raw_transaction
            && latest_wallet.expiry_height == prepared.expiry_height,
        "payment {activity_id} wallet preparation changed after original reconciliation"
    );
    Ok(true)
}

pub(crate) async fn repair_activity_journal(
    state: &AppState,
    activity: &Activity,
) -> anyhow::Result<Option<Activity>> {
    let Some(prepared) = state.0.wallet.prepared(&activity.id).await? else {
        return Ok(None);
    };
    let repaired = state.0.store.record_prepared(
        &activity.id,
        &prepared.txid,
        &prepared.raw_transaction,
        prepared.expiry_height,
    )?;
    Ok(Some(repaired))
}

pub(crate) async fn prepare_activity_intent(
    attempt: &PaymentAttempt<'_>,
    intent: &crate::db::ActivityIntent,
    replacement_txid: Option<&str>,
) -> anyhow::Result<PreparedPayment> {
    let state = attempt.state;
    let activity = &intent.activity;
    attempt
        .synchronize_wallet(None)
        .await
        .context("synchronizing wallet for payment preparation")?;
    let seed = state.0.store.seed()?;
    let record = record_prepared_payment(&state.0.store, &activity.id);
    match activity.kind.as_str() {
        "send" => {
            let account_id = activity
                .from_account
                .context("send omitted source account")?;
            let destination = if let Some(address) = &activity.to_address {
                address.clone()
            } else {
                let account = state
                    .0
                    .store
                    .account(activity.to_account.context("send omitted destination")?)?;
                match activity.destination_pool.as_str() {
                    "transparent" => account.transparent_address,
                    "ironwood" => account.unified_address,
                    pool => anyhow::bail!("send has unsupported destination pool {pool}"),
                }
            };
            let memo = parse_payment_memo(intent.memo.as_deref(), &activity.destination_pool)
                .map_err(|error| anyhow::anyhow!(error))?;
            attempt.check_can_start()?;
            state
                .0
                .wallet
                .prepare(
                    Some(&activity.id),
                    replacement_txid,
                    &seed,
                    account_id,
                    &activity.source_pool,
                    &destination,
                    activity.amount_zatoshi,
                    memo,
                    record,
                )
                .await
        }
        "faucet" => {
            let account = state.0.store.account(TREASURY_ACCOUNT_ID)?;
            let destination = state.0.store.account(
                activity
                    .to_account
                    .context("faucet omitted destination account")?,
            )?;
            let address = match activity.destination_pool.as_str() {
                "transparent" => destination.transparent_address,
                "ironwood" => destination.unified_address,
                pool => anyhow::bail!("faucet has unsupported destination pool {pool}"),
            };
            let _replenishment = attempt.lock(&state.0.treasury_replenishment).await?;
            prepare_with_replenishment(
                attempt,
                Some(&activity.id),
                replacement_txid,
                &seed,
                &account,
                &address,
                activity.amount_zatoshi,
                record,
            )
            .await
        }
        kind => anyhow::bail!("unsupported accepted payment kind {kind}"),
    }
}

pub(crate) async fn progress_address_faucet(
    state: &AppState,
    payment: AddressFaucet,
    deadline: Instant,
) -> anyhow::Result<()> {
    execute_address_payment(
        &PaymentAttempt {
            state,
            dispatch_deadline: Some(deadline),
        },
        payment,
    )
    .await?;
    Ok(())
}

pub(crate) async fn execute_address_payment(
    attempt: &PaymentAttempt<'_>,
    mut payment: AddressFaucet,
) -> anyhow::Result<AddressFaucet> {
    let state = attempt.state;
    let mut replacement_txid = None;
    loop {
        attempt.check_can_start()?;
        match payment.status.as_str() {
            "confirmed" | "failed" => return Ok(payment),
            "unconfirmed" => {
                return confirm_address_after_mining_with_attempt(attempt, payment).await;
            }
            "prepared" | "broadcast" => {
                match submit_address_prepared(&state.0.store, attempt, &payment).await? {
                    AddressSubmission::Broadcast(updated) => {
                        return confirm_address_after_mining_with_attempt(attempt, updated).await;
                    }
                    AddressSubmission::Expired(updated) => {
                        replacement_txid = Some(payment.txid.clone());
                        payment = updated;
                    }
                }
            }
            "preparing" => {
                if replacement_txid.is_none()
                    && let Some(prepared) = repair_address_journal(state, &payment).await?
                {
                    payment = prepared;
                    continue;
                }
                attempt
                    .synchronize_wallet(None)
                    .await
                    .context("synchronizing wallet for address faucet preparation")?;
                let seed = state.0.store.seed()?;
                let treasury = state.0.store.account(TREASURY_ACCOUNT_ID)?;
                let _replenishment = attempt.lock(&state.0.treasury_replenishment).await?;
                let prepared = prepare_with_replenishment(
                    attempt,
                    Some(&payment.id),
                    replacement_txid.as_deref(),
                    &seed,
                    &treasury,
                    &payment.address,
                    payment.amount_zatoshi,
                    record_prepared_address_payment(&state.0.store, &payment.id),
                )
                .await
                .context("preparing address faucet with replenishment")?;
                replacement_txid = None;
                payment = state.0.store.record_address_prepared(
                    &payment.id,
                    &prepared.txid,
                    &prepared.raw_transaction,
                    prepared.expiry_height,
                )?;
            }
            status => anyhow::bail!("address faucet has unsupported status {status}"),
        }
    }
}

pub(crate) async fn repair_address_journal(
    state: &AppState,
    payment: &AddressFaucet,
) -> anyhow::Result<Option<AddressFaucet>> {
    let Some(prepared) = state.0.wallet.prepared(&payment.id).await? else {
        return Ok(None);
    };
    let repaired = state.0.store.record_address_prepared(
        &payment.id,
        &prepared.txid,
        &prepared.raw_transaction,
        prepared.expiry_height,
    )?;
    Ok(Some(repaired))
}

#[cfg(test)]
#[path = "../../tests/unit/payments/progression/tests.rs"]
pub(crate) mod tests;
