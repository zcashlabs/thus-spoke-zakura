use super::*;

const TRUNCATED_RPC_RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1024\r\nConnection: close\r\n\r\n{";

fn complete_rpc_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

async fn rpc_responses(
    responses: Vec<String>,
) -> (
    String,
    tokio::task::JoinHandle<()>,
    tokio::sync::watch::Receiver<usize>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (sent, progress) = tokio::sync::watch::channel(0);
    let server = tokio::spawn(async move {
        for (index, response) in responses.into_iter().enumerate() {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                assert!(stream.read_buf(&mut request).await.unwrap() > 0);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    if headers.starts_with("HEAD ") {
                        // Local health probes can connect to ephemeral test ports.
                        stream = listener.accept().await.unwrap().0;
                        request.clear();
                        continue;
                    }
                    let length: usize = headers
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .unwrap()
                        .1
                        .trim()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
            sent.send_replace(index + 1);
        }
    });
    (endpoint, server, progress)
}

async fn assert_startup_recovers_from_rpc_disconnect(response: &str) {
    let (endpoint, server, _) = rpc_responses(vec![
        response.to_owned(),
        complete_rpc_response(r#"{"result":{"chain":"regtest","blocks":1},"error":null,"id":0}"#),
    ])
    .await;
    let rpc = rpc::NodeRpc::new(endpoint);
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        retry_startup(|| async { rpc.chain_info().await.map(|_| ()) }),
    )
    .await;
    server.abort();
    if let Err(error) = server.await {
        assert!(error.is_cancelled());
    }
    result.unwrap().unwrap();
}

#[tokio::test]
async fn startup_recovers_when_rpc_disconnects_before_response_headers() {
    assert_startup_recovers_from_rpc_disconnect("").await;
}

#[tokio::test]
async fn startup_recovers_when_rpc_disconnects_during_the_response_body() {
    assert_startup_recovers_from_rpc_disconnect(TRUNCATED_RPC_RESPONSE).await;
}

#[tokio::test]
async fn invalid_rpc_endpoints_remain_fatal_during_startup() {
    for endpoint in ["://invalid", "ftp://127.0.0.1:1"] {
        let error = rpc::NodeRpc::new(endpoint.into())
            .chain_info()
            .await
            .unwrap_err();
        assert!(!startup_retryable(&error), "{error:#}");
    }
}

#[tokio::test]
async fn stored_status_survives_repeated_rpc_response_disconnects() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("ths.db")).unwrap();
    store.initialize().unwrap();
    let activity = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "disconnect-send",
            None,
        )
        .unwrap();
    let faucet = store
        .claim_address_faucet("tm-external", 7, "disconnect-faucet")
        .unwrap();
    store
        .record_address_error(&faucet.id, "preparing", "", "awaiting node")
        .unwrap();
    let wallet = wallet::RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    let (endpoint, rpc_server, mut progress) =
        rpc_responses(vec![TRUNCATED_RPC_RESPONSE.to_owned(); 3]).await;
    let state = AppState::new(store.clone(), wallet.clone(), endpoint, "test".into());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve_with_background(
        listener,
        api::router(state.clone()),
        background_tasks(state),
    ));
    let observed = tokio::time::timeout(Duration::from_secs(5), async {
        while *progress.borrow_and_update() < 3 {
            progress.changed().await?;
        }
        Ok::<_, tokio::sync::watch::error::RecvError>(())
    })
    .await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let result = async {
        observed.context("startup did not retry the interrupted RPC")??;
        let response = client
            .get(format!(
                "http://{address}/api/v1/activity?idempotency_key=disconnect-send"
            ))
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = response.json().await?;
        assert_eq!(body[0]["id"], activity.id);
        assert_eq!(body[0]["status"], "preparing");
        assert_eq!(body[0]["txid"], "");
        let response = client
            .get(format!(
                "http://{address}/api/v1/faucet/address?idempotency_key=disconnect-faucet"
            ))
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = response.json().await?;
        assert_eq!(body["status"], "preparing");
        assert_eq!(body["last_error"], "awaiting node");
        let health = client
            .get(format!("http://{address}/api/v1/health"))
            .send()
            .await?;
        assert_eq!(health.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert!(!server.is_finished());
        assert!(store.prepared_transaction(&activity.id).is_err());
        assert!(wallet.prepared(&activity.id).await?.is_none());
        Ok::<_, anyhow::Error>(())
    }
    .await;
    server.abort();
    if let Err(error) = server.await {
        assert!(error.is_cancelled());
    }
    rpc_server.abort();
    if let Err(error) = rpc_server.await {
        assert!(error.is_cancelled());
    }
    result.unwrap();
}

#[tokio::test]
async fn complete_malformed_rpc_json_stops_http_during_startup() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("ths.db")).unwrap();
    store.initialize().unwrap();
    let wallet = wallet::RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    let (endpoint, rpc_server, _) = rpc_responses(vec![complete_rpc_response("{")]).await;
    let state = AppState::new(store, wallet, endpoint, "test".into());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        serve_with_background(
            listener,
            api::router(state.clone()),
            background_tasks(state),
        ),
    )
    .await;
    rpc_server.abort();
    if let Err(error) = rpc_server.await {
        assert!(error.is_cancelled());
    }
    let error = result.unwrap().unwrap_err();
    assert!(!startup_retryable(&error));
    assert!(error.chain().any(|cause| cause.is::<serde_json::Error>()));
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn startup_retries_past_two_minutes_with_a_delay_then_recovers() {
    for code in [tonic::Code::Unavailable, tonic::Code::Aborted] {
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = attempts.clone();
        let started = tokio::time::Instant::now();
        retry_startup(|| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {
                if started.elapsed() < Duration::from_secs(125) {
                    Err(tonic::Status::new(code, "dependency repairing its chain view").into())
                } else {
                    Ok(())
                }
            }
        })
        .await
        .unwrap();
        assert!(started.elapsed() >= Duration::from_secs(125));
        assert!(attempts.load(std::sync::atomic::Ordering::SeqCst) < 180);
    }
}

#[test]
fn startup_retries_only_typed_transient_failures() {
    for code in [-28, -5, -32603] {
        let rpc: rpc::RpcError =
            serde_json::from_value(serde_json::json!({"code": code, "message": "waiting"}))
                .unwrap();
        assert_eq!(
            startup_retryable(&anyhow::Error::new(rpc).context("startup")),
            code == -28
        );
    }
    for code in [
        tonic::Code::Unavailable,
        tonic::Code::DeadlineExceeded,
        tonic::Code::ResourceExhausted,
        tonic::Code::Aborted,
        tonic::Code::InvalidArgument,
        tonic::Code::DataLoss,
        tonic::Code::Internal,
        tonic::Code::Unknown,
    ] {
        let error = anyhow::Error::new(tonic::Status::new(code, "waiting"));
        assert_eq!(
            startup_retryable(&error),
            matches!(
                code,
                tonic::Code::Unavailable
                    | tonic::Code::DeadlineExceeded
                    | tonic::Code::ResourceExhausted
                    | tonic::Code::Aborted
            )
        );
    }
    for error in [
        anyhow::anyhow!("node unavailable"),
        anyhow::anyhow!("invalid Regtest checkpoint"),
        rusqlite::Error::InvalidQuery.into(),
        db::IdempotencyConflict.into(),
        wallet::PaymentError::TreasuryExhausted.into(),
        wallet::PaymentError::ScanRequired.into(),
    ] {
        assert!(!startup_retryable(&error), "{error}");
    }
    for pending in [
        ReadinessError::BelowBirthday,
        ReadinessError::RewindCheckpoint,
        ReadinessError::CheckpointChanged,
        ReadinessError::FundingPending,
    ] {
        assert!(startup_retryable(&pending.into()));
    }
    assert!(startup_retryable(&reconcile::ReconciliationDeadline.into()));
}

#[tokio::test]
async fn fatal_startup_error_stops_http_and_cancels_siblings() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (cancelled, cancellation) = tokio::sync::oneshot::channel::<()>();
    let mut background = tokio::task::JoinSet::new();
    background.spawn(async move {
        let _cancelled = cancelled;
        std::future::pending::<Result<()>>().await
    });
    background.spawn(retry_startup(|| async {
        Err(rusqlite::Error::InvalidQuery.into())
    }));
    let error = serve_with_background(listener, axum::Router::new(), background)
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<rusqlite::Error>().is_some());
    assert!(cancellation.await.is_err());
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}

async fn assert_background_failure_stops_http(panics: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new().route("/", axum::routing::get(|| async { "ready" }));
    let mut background = tokio::task::JoinSet::new();
    let (cancelled, cancellation) = tokio::sync::oneshot::channel::<()>();
    let (started, ready) = tokio::sync::oneshot::channel();
    background.spawn(async move {
        // Dropping this sender proves the sibling task was cancelled.
        let _cancelled = cancelled;
        started.send(()).unwrap();
        std::future::pending::<Result<()>>().await
    });
    ready.await.unwrap();
    let (exit, exiting) = tokio::sync::oneshot::channel();
    background.spawn(async move {
        exiting.await.unwrap();
        assert!(!panics, "injected recovery task panic");
        Ok(())
    });
    let server = tokio::spawn(serve_with_background(listener, app, background));
    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
        .get(format!("http://{address}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    exit.send(()).unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    if panics {
        assert!(
            error
                .downcast_ref::<tokio::task::JoinError>()
                .unwrap()
                .is_panic()
        );
    } else {
        assert_eq!(
            error.to_string(),
            "server background task unexpectedly exited"
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(2), cancellation)
            .await
            .unwrap()
            .is_err()
    );
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}

#[tokio::test]
async fn background_task_exit_stops_http_and_cancels_siblings() {
    assert_background_failure_stops_http(false).await;
}

#[tokio::test]
async fn background_task_panic_stops_http_and_cancels_siblings() {
    assert_background_failure_stops_http(true).await;
}

#[tokio::test(start_paused = true)]
async fn stored_payments_are_served_after_restart_while_dependencies_are_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("ths.db")).unwrap();
    store.initialize().unwrap();
    let failed = store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            12_000,
            "restart-failed",
            None,
        )
        .unwrap();
    store.fail(&failed.id, "insufficient funds").unwrap();
    let external = store
        .claim_address_faucet("tm-external", 7, "restart-external")
        .unwrap();
    store
        .record_address_error(&external.id, "preparing", "", "node unavailable")
        .unwrap();
    drop(store);

    let store = Store::open(dir.path().join("ths.db")).unwrap();
    store.initialize().unwrap();
    let wallet = wallet::RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    let state = AppState::new(
        store.clone(),
        wallet,
        "http://127.0.0.1:1".into(),
        "test".into(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve_with_background(
        listener,
        api::router(state.clone()),
        background_tasks(state),
    ));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let get = |path: &str| {
        let request = client.get(format!("http://{address}{path}"));
        async move {
            let response = request.send().await.unwrap();
            let status = response.status();
            (status, response.json::<serde_json::Value>().await.unwrap())
        }
    };

    tokio::time::sleep(Duration::from_secs(122)).await;
    // Startup retries outlast the former two-minute deadline in virtual time.
    // Real time resumes so the client's 2 s timeout cannot fire by auto-advance.
    tokio::time::resume();
    let (status, activity) = get("/api/v1/activity?idempotency_key=restart-failed").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(activity[0]["id"], failed.id);
    assert_eq!(activity[0]["status"], "failed");
    assert_eq!(activity[0]["last_error"], "insufficient funds");
    let (status, faucet) = get("/api/v1/faucet/address?idempotency_key=restart-external").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(faucet["status"], "preparing");
    assert_eq!(faucet["last_error"], "node unavailable");
    let (status, health) = get("/api/v1/health").await;
    assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(health["ok"], false);
    let response = client
        .post(format!("http://{address}/api/v1/faucet"))
        .json(&serde_json::json!({"account_id": 2, "pool": "ironwood", "amount_zatoshi": 1_000, "idempotency_key": "restart-faucet"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert!(store.activity_for_key("restart-faucet").unwrap().is_none());
    assert!(!server.is_finished());
    server.abort();
}

use crate::{db, reconcile, rpc};
