use super::attempt::PaymentAttempt;
use super::confirmation::confirm_after_mining_with_attempt;
use super::progression::can_fail_insufficient_send_after_preparation;
use super::progression::is_insufficient_funds;
use super::progression::notify_activity_change;
use super::progression::repair_activity_journal;
use super::submission::PreparedSubmission;
use super::submission::submit_prepared;
use crate::db::Activity;
use crate::db::Store;
#[cfg(test)]
use crate::state::AppState;
use crate::state::notify;
use crate::wallet::PaymentError;
use crate::wallet::PreparedPayment;
use std::future::Future;
use zcash_protocol::memo::MemoBytes;

#[async_trait::async_trait]
pub(crate) trait PaymentLifecycle: Sync {
    fn check_can_start(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn repair_preparation(&self, activity: &Activity) -> anyhow::Result<Option<Activity>> {
        let _ = activity;
        Ok(None)
    }
    async fn submit_prepared(&self, activity: &Activity) -> anyhow::Result<PreparedSubmission>;
    fn record_preparation_error(&self, id: &str, error: &str) -> anyhow::Result<()>;
    fn record_prepared(&self, id: &str, prepared: &PreparedPayment) -> anyhow::Result<Activity>;
    async fn confirm_after_mining(&self, activity: Activity) -> anyhow::Result<Activity>;
    async fn fail_insufficient_send(
        &self,
        activity: &Activity,
        error: &str,
        replacement_txid: Option<&str>,
    ) -> anyhow::Result<bool> {
        let _ = (activity, error, replacement_txid);
        Ok(false)
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl PaymentLifecycle for AppState {
    async fn repair_preparation(&self, activity: &Activity) -> anyhow::Result<Option<Activity>> {
        repair_activity_journal(self, activity).await
    }

    async fn submit_prepared(&self, activity: &Activity) -> anyhow::Result<PreparedSubmission> {
        PaymentLifecycle::submit_prepared(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            activity,
        )
        .await
    }

    fn record_preparation_error(&self, id: &str, error: &str) -> anyhow::Result<()> {
        PaymentLifecycle::record_preparation_error(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            id,
            error,
        )
    }

    fn record_prepared(&self, id: &str, prepared: &PreparedPayment) -> anyhow::Result<Activity> {
        PaymentLifecycle::record_prepared(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            id,
            prepared,
        )
    }

    async fn confirm_after_mining(&self, activity: Activity) -> anyhow::Result<Activity> {
        PaymentLifecycle::confirm_after_mining(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            activity,
        )
        .await
    }

    async fn fail_insufficient_send(
        &self,
        activity: &Activity,
        error: &str,
        replacement_txid: Option<&str>,
    ) -> anyhow::Result<bool> {
        PaymentLifecycle::fail_insufficient_send(
            &PaymentAttempt {
                state: self,
                dispatch_deadline: None,
            },
            activity,
            error,
            replacement_txid,
        )
        .await
    }
}

#[async_trait::async_trait]
impl PaymentLifecycle for PaymentAttempt<'_> {
    fn check_can_start(&self) -> anyhow::Result<()> {
        PaymentAttempt::check_can_start(self)
    }

    async fn repair_preparation(&self, activity: &Activity) -> anyhow::Result<Option<Activity>> {
        repair_activity_journal(self.state, activity).await
    }

    async fn submit_prepared(&self, activity: &Activity) -> anyhow::Result<PreparedSubmission> {
        let current = self.state.0.store.activity(&activity.id)?;
        let result = submit_prepared(&self.state.0.store, self, activity).await?;
        notify_activity_change(self.state, &current)?;
        Ok(result)
    }

    fn record_preparation_error(&self, id: &str, error: &str) -> anyhow::Result<()> {
        let current = self.state.0.store.activity(id)?;
        let updated = self
            .state
            .0
            .store
            .record_error(id, &current.status, &current.txid, error)?;
        if updated != current {
            notify(self.state, "wallet");
        }
        Ok(())
    }

    fn record_prepared(&self, id: &str, prepared: &PreparedPayment) -> anyhow::Result<Activity> {
        let current = self.state.0.store.activity(id)?;
        let updated = self.state.0.store.record_prepared(
            id,
            &prepared.txid,
            &prepared.raw_transaction,
            prepared.expiry_height,
        )?;
        if updated != current {
            notify(self.state, "wallet");
        }
        Ok(updated)
    }

    async fn confirm_after_mining(&self, activity: Activity) -> anyhow::Result<Activity> {
        confirm_after_mining_with_attempt(self, activity).await
    }

    async fn fail_insufficient_send(
        &self,
        activity: &Activity,
        error: &str,
        replacement_txid: Option<&str>,
    ) -> anyhow::Result<bool> {
        if activity.kind != "send"
            || activity.status != "preparing"
            || !activity.txid.is_empty()
            || !can_fail_insufficient_send_after_preparation(self, &activity.id, replacement_txid)
                .await?
        {
            return Ok(false);
        }
        let current = self.state.0.store.activity(&activity.id)?;
        let updated = self.state.0.store.fail(&activity.id, error)?;
        if updated != current {
            notify(self.state, "wallet");
        }
        Ok(true)
    }
}

pub(crate) async fn execute_payment<R, Prepare, PrepareFuture>(
    runtime: &R,
    pending: Activity,
    prepare: Prepare,
) -> anyhow::Result<Activity>
where
    R: PaymentLifecycle,
    Prepare: FnMut(String, Option<String>) -> PrepareFuture,
    PrepareFuture: Future<Output = anyhow::Result<PreparedPayment>>,
{
    let id = pending.id.clone();
    let result = execute_payment_inner(runtime, pending, prepare).await;
    if let Err(error) = &result
        && let Err(persistence_error) = runtime.record_preparation_error(&id, &format!("{error:#}"))
    {
        tracing::warn!(
            error = %format_args!("{persistence_error:#}"),
            execution_error = %format_args!("{error:#}"),
            activity = %id,
            "could not record payment execution error"
        );
    }
    result
}

pub(crate) async fn execute_payment_inner<R, Prepare, PrepareFuture>(
    runtime: &R,
    mut pending: Activity,
    mut prepare: Prepare,
) -> anyhow::Result<Activity>
where
    R: PaymentLifecycle,
    Prepare: FnMut(String, Option<String>) -> PrepareFuture,
    PrepareFuture: Future<Output = anyhow::Result<PreparedPayment>>,
{
    let mut replacement_txid = None;
    loop {
        runtime.check_can_start()?;
        match pending.status.as_str() {
            "confirmed" | "failed" => return Ok(pending),
            "unconfirmed" => return runtime.confirm_after_mining(pending).await,
            "prepared" | "broadcast" => match runtime.submit_prepared(&pending).await? {
                PreparedSubmission::Broadcast(activity) => {
                    return runtime.confirm_after_mining(activity).await;
                }
                PreparedSubmission::Expired(activity) => {
                    replacement_txid = Some(pending.txid.clone());
                    pending = activity;
                }
            },
            "preparing" => {
                if replacement_txid.is_none()
                    && let Some(repaired) = runtime.repair_preparation(&pending).await?
                {
                    pending = repaired;
                    continue;
                }
                runtime.check_can_start()?;
                let replacement_txid = replacement_txid.take();
                let prepared = match prepare(pending.id.clone(), replacement_txid.clone()).await {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        let error = if pending.kind == "send" && is_insufficient_funds(&error) {
                            match runtime
                                .fail_insufficient_send(
                                    &pending,
                                    &format!("{error:#}"),
                                    replacement_txid.as_deref(),
                                )
                                .await
                            {
                                Ok(true) => return Err(error),
                                Ok(false) => error,
                                Err(reconciliation_error) => reconciliation_error.context(format!(
                                    "reconciling send preparation failure: {error:#}"
                                )),
                            }
                        } else {
                            error
                        };
                        return Err(error.context("preparing payment"));
                    }
                };
                pending = runtime.record_prepared(&pending.id, &prepared)?;
            }
            status => anyhow::bail!("payment has unsupported status {status}"),
        }
    }
}

pub(crate) fn record_prepared_payment(
    store: &Store,
    id: &str,
) -> impl FnOnce(PreparedPayment) -> anyhow::Result<PreparedPayment> + Clone + Send + 'static {
    let store = store.clone();
    let id = id.to_owned();
    move |prepared| {
        store.record_prepared(
            &id,
            &prepared.txid,
            &prepared.raw_transaction,
            prepared.expiry_height,
        )?;
        Ok(prepared)
    }
}

/// An absent (or null) memo is `None`. Any present memo, including `""`, is
/// encoded as an explicit ZIP-302 text memo and is only valid for ironwood.
pub(crate) fn parse_payment_memo(
    memo: Option<&str>,
    destination_pool: &str,
) -> Result<Option<MemoBytes>, String> {
    let Some(text) = memo else {
        return Ok(None);
    };
    if destination_pool != "ironwood" {
        return Err(PaymentError::TransparentMemo.to_string());
    }
    // Text memos are zero-padded to 512 bytes, so a trailing NUL could not be
    // told apart from padding and would be silently lost on decode.
    if text.ends_with('\0') {
        return Err("memo must not end with a NUL (U+0000) character".into());
    }
    MemoBytes::from_bytes(text.as_bytes())
        .map(Some)
        .map_err(|error| format!("invalid memo: {error}"))
}

#[cfg(test)]
#[path = "../../tests/unit/payments/execution/tests.rs"]
pub(crate) mod tests;
