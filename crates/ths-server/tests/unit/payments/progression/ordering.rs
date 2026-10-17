use super::*;
use crate::payments::execution::PaymentLifecycle;
use crate::payments::submission::PreparedSubmission;

fn dependent_send(store: &Store, key: &str, pool: &str) -> Activity {
    store
        .claim_transfer(
            2,
            Recipient::Account(3),
            pool,
            "ironwood",
            12_000,
            key,
            None,
        )
        .unwrap()
}

fn insufficient() -> anyhow::Result<PreparedPayment> {
    Err(PaymentError::InsufficientFunds {
        available: 0,
        required: 22_000,
    }
    .into())
}

struct FundingLifecycle {
    state: AppState,
    funded: AtomicBool,
}

#[async_trait::async_trait]
impl PaymentLifecycle for FundingLifecycle {
    async fn submit_prepared(&self, activity: &Activity) -> anyhow::Result<PreparedSubmission> {
        Ok(PreparedSubmission::Broadcast(
            self.state
                .0
                .store
                .mark_broadcast(&activity.id, &activity.txid)?,
        ))
    }

    fn record_preparation_error(&self, id: &str, error: &str) -> anyhow::Result<()> {
        let current = self.state.0.store.activity(id)?;
        self.state
            .0
            .store
            .record_error(id, &current.status, &current.txid, error)?;
        Ok(())
    }

    fn record_prepared(&self, id: &str, prepared: &PreparedPayment) -> anyhow::Result<Activity> {
        self.state.0.store.record_prepared(
            id,
            &prepared.txid,
            &prepared.raw_transaction,
            prepared.expiry_height,
        )
    }

    async fn confirm_after_mining(&self, activity: Activity) -> anyhow::Result<Activity> {
        if activity.kind == "faucet" {
            self.funded.store(true, Ordering::SeqCst);
        }
        self.state
            .0
            .store
            .confirm(&activity.id, &activity.txid, "fixture-block")
    }

    async fn fail_insufficient_send(
        &self,
        activity: &Activity,
        error: &str,
        replacement_txid: Option<&str>,
    ) -> anyhow::Result<bool> {
        PaymentLifecycle::fail_insufficient_send(&self.state, activity, error, replacement_txid)
            .await
    }
}

#[tokio::test]
async fn dependent_send_keeps_its_key_when_a_pass_resumes_ahead_of_earlier_funding() {
    let (state, _dir) = state_with_local_wallet();
    let funding = state
        .0
        .store
        .claim_faucet(2, "ironwood", 100_000, "earlier-funding")
        .unwrap();
    let send = dependent_send(&state.0.store, "dependent-send", "ironwood");
    let runtime = FundingLifecycle {
        state,
        funded: AtomicBool::new(false),
    };

    // A rotated cursor or exhausted previous pass can reach the send first.
    assert!(
        execute_payment(&runtime, send.clone(), |_, _| std::future::ready(
            insufficient()
        ))
        .await
        .is_err()
    );
    let retained = runtime
        .state
        .0
        .store
        .activity_for_key("dependent-send")
        .unwrap()
        .unwrap();
    assert_eq!(retained.id, send.id);
    assert_eq!(retained.status, "preparing");
    assert!(retained.txid.is_empty());
    assert!(
        retained
            .last_error
            .as_deref()
            .unwrap()
            .contains("insufficient")
    );

    execute_payment(&runtime, funding, |_, _| {
        std::future::ready(Ok(PreparedPayment {
            txid: "funding-fixture".into(),
            raw_transaction: b"funding".to_vec(),
            expiry_height: 140,
        }))
    })
    .await
    .unwrap();
    let replay = dependent_send(&runtime.state.0.store, "dependent-send", "ironwood");
    let confirmed = execute_payment(&runtime, replay, |_, _| {
        assert!(runtime.funded.load(Ordering::SeqCst));
        std::future::ready(Ok(PreparedPayment {
            txid: "dependent-fixture".into(),
            raw_transaction: b"dependent".to_vec(),
            expiry_height: 140,
        }))
    })
    .await
    .unwrap();
    assert_eq!(confirmed.id, send.id);
    assert_eq!(confirmed.status, "confirmed");
    assert_eq!(confirmed.last_error, None);
    let replay = dependent_send(&runtime.state.0.store, "dependent-send", "ironwood");
    let prepares = AtomicUsize::new(0);
    assert_eq!(
        execute_payment(&runtime, replay, |_, _| {
            prepares.fetch_add(1, Ordering::SeqCst);
            std::future::ready(insufficient())
        })
        .await
        .unwrap(),
        confirmed
    );
    assert_eq!(prepares.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn only_earlier_matching_unresolved_funding_defers_insufficient_failure() {
    for case in [
        "incoming-faucet",
        "incoming-send",
        "incoming-address",
        "transparent-change",
        "wrong-pool",
        "wrong-account",
        "later",
        "confirmed",
        "failed",
    ] {
        let (state, _dir) = state_with_local_wallet();
        let later_send =
            (case == "later").then(|| dependent_send(&state.0.store, "matching-send", "ironwood"));
        let funding = if case == "incoming-send" {
            state
                .0
                .store
                .claim_transfer(
                    1,
                    Recipient::Account(2),
                    "ironwood",
                    "ironwood",
                    100_000,
                    "funder",
                    None,
                )
                .unwrap()
        } else if case == "incoming-address" {
            let address = state.0.store.account(2).unwrap().unified_address;
            state
                .0
                .store
                .claim_transfer(
                    1,
                    Recipient::Address(&address),
                    "ironwood",
                    "ironwood",
                    100_000,
                    "funder",
                    None,
                )
                .unwrap()
        } else if case == "transparent-change" {
            state
                .0
                .store
                .claim_transfer(
                    2,
                    Recipient::Account(4),
                    "transparent",
                    "transparent",
                    100_000,
                    "funder",
                    None,
                )
                .unwrap()
        } else {
            state
                .0
                .store
                .claim_faucet(
                    if case == "wrong-account" { 4 } else { 2 },
                    if case == "wrong-pool" {
                        "transparent"
                    } else {
                        "ironwood"
                    },
                    100_000,
                    "funder",
                )
                .unwrap()
        };
        if case == "confirmed" {
            state
                .0
                .store
                .record_prepared(&funding.id, "funding", b"signed", 140)
                .unwrap();
            state
                .0
                .store
                .confirm(&funding.id, "funding", "block")
                .unwrap();
        } else if case == "failed" {
            state.0.store.fail(&funding.id, "fixture failure").unwrap();
        }
        let send = later_send
            .unwrap_or_else(|| dependent_send(&state.0.store, "matching-send", "ironwood"));
        assert!(
            execute_payment(&state, send.clone(), |_, _| std::future::ready(
                insufficient()
            ))
            .await
            .is_err()
        );
        let expected = if matches!(
            case,
            "incoming-faucet" | "incoming-send" | "incoming-address" | "transparent-change"
        ) {
            "preparing"
        } else {
            "failed"
        };
        assert_eq!(
            state.0.store.activity(&send.id).unwrap().status,
            expected,
            "{case}"
        );
    }
}

async fn ordering_rpc() -> (
    String,
    tokio::task::JoinHandle<()>,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorded = calls.clone();
    let app = Router::new().route(
        "/",
        post(move |Json(request): Json<Value>| {
            let recorded = recorded.clone();
            async move {
                let result = match request["method"].as_str().unwrap() {
                    "getrawtransaction" => {
                        let txid = request["params"][0].as_str().unwrap();
                        recorded.lock().unwrap().push(txid.to_owned());
                        json!({"txid":txid,"confirmations":1,"blockhash":"ordered-block"})
                    }
                    "getblock" => json!({"height":100}),
                    "getblockhash" => json!("ordered-block"),
                    method => panic!("unexpected ordering RPC {method}"),
                };
                Json(json!({"result":result,"error":null,"id":request["id"]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (endpoint, server, calls)
}

#[tokio::test]
async fn worker_uses_admission_order_even_when_payment_ids_sort_backwards() {
    let (endpoint, server, calls) = ordering_rpc().await;
    let (mut state, dir) = state_with_local_wallet();
    Arc::get_mut(&mut state.0).unwrap().rpc = NodeRpc::new(endpoint);
    let funding = state
        .0
        .store
        .claim_faucet(2, "ironwood", 100_000, "ordered-funding")
        .unwrap();
    let send = dependent_send(&state.0.store, "ordered-send", "ironwood");
    // Deterministic reversed IDs and tied timestamps must not change admission order.
    let db = rusqlite::Connection::open(dir.path().join("server.db")).unwrap();
    for (claim, id, txid) in [
        (&funding, "ffffffff-funder", "funding-tx"),
        (&send, "00000000-send", "send-tx"),
    ] {
        db.execute(
            "UPDATE activity SET id=?1,created_at='2026-10-17 00:00:00' WHERE id=?2",
            rusqlite::params![id, claim.id],
        )
        .unwrap();
        db.execute(
            "UPDATE idempotency SET activity_id=?1 WHERE activity_id=?2",
            rusqlite::params![id, claim.id],
        )
        .unwrap();
        state
            .0
            .store
            .record_prepared(id, txid, b"signed fixture", 140)
            .unwrap();
    }
    let mut traversal = PaymentTraversal::default();
    progress_payment_intents_with_traversal(
        &state,
        Instant::now() + PAYMENT_DISPATCH_BUDGET,
        &mut traversal,
    )
    .await
    .unwrap();
    let mut observed = calls.lock().unwrap().clone();
    observed.dedup();
    assert_eq!(observed, ["funding-tx", "send-tx"]);
    assert_eq!(
        state
            .0
            .store
            .activity_for_key("ordered-funding")
            .unwrap()
            .unwrap()
            .status,
        "confirmed"
    );
    assert_eq!(
        state
            .0
            .store
            .activity_for_key("ordered-send")
            .unwrap()
            .unwrap()
            .status,
        "confirmed"
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn admission_order_survives_reopen_legacy_interleaving_and_an_exhausted_pass() {
    let (endpoint, server, calls) = ordering_rpc().await;
    let (state, dir) = state_with_local_wallet();
    let funding = state
        .0
        .store
        .claim_faucet(2, "ironwood", 100_000, "restart-funder")
        .unwrap();
    let legacy = dependent_send(&state.0.store, "restart-legacy", "ironwood");
    let dependent = dependent_send(&state.0.store, "restart-dependent", "ironwood");
    let db = rusqlite::Connection::open(dir.path().join("server.db")).unwrap();
    for (claim, id, txid) in [
        (&funding, "ffffffff-funder", "funding-tx"),
        (&legacy, "eeeeeeee-legacy", "legacy-tx"),
        (&dependent, "00000000-dependent", "dependent-tx"),
    ] {
        db.execute(
            "UPDATE activity SET id=?1,created_at='2026-10-17 00:00:00' WHERE id=?2",
            rusqlite::params![id, claim.id],
        )
        .unwrap();
        db.execute(
            "UPDATE idempotency SET activity_id=?1 WHERE activity_id=?2",
            rusqlite::params![id, claim.id],
        )
        .unwrap();
        state
            .0
            .store
            .record_prepared(id, txid, b"retained bytes", 140)
            .unwrap();
    }
    db.execute("DELETE FROM idempotency WHERE key='restart-legacy'", [])
        .unwrap();
    drop(db);
    drop(state);
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    let state = AppState::new(store, wallet, endpoint, "reopened-ordering".into());
    state.mark_started();
    let mut traversal = PaymentTraversal::default();

    // Exhaust a real pass waiting for the payment lock. Its cursor must retain
    // the first admission's position without preventing the others next time.
    let lock = state.0.payments.lock().await;
    tokio::time::pause();
    progress_payment_intents_with_traversal(
        &state,
        Instant::now() + Duration::from_millis(25),
        &mut traversal,
    )
    .await
    .unwrap();
    tokio::time::resume();
    assert!(calls.lock().unwrap().is_empty());
    drop(lock);
    progress_payment_intents_with_traversal(
        &state,
        Instant::now() + PAYMENT_DISPATCH_BUDGET,
        &mut traversal,
    )
    .await
    .unwrap();
    let mut observed = calls.lock().unwrap().clone();
    observed.dedup();
    assert_eq!(observed, ["legacy-tx", "dependent-tx", "funding-tx"]);
    for id in ["ffffffff-funder", "eeeeeeee-legacy", "00000000-dependent"] {
        assert_eq!(state.0.store.activity(id).unwrap().status, "confirmed");
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}
