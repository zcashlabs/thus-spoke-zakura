mod payment_proxy;
mod regtest;
mod rpc_proxy;

use std::time::Duration;

use anyhow::{Result, bail};
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

pub use payment_proxy::{QueuedBroadcastProxy, lose_payment_response};
pub use regtest::{
    FailureRoute, HeightCheckpoint, RecoveryFailureReporter, RecoveryPhase, RegtestStack,
    TerminationSignals,
};
pub use rpc_proxy::{GenerateCounts, GenerateFaultProxy};

const MAX_JSON_BODY_BYTES: usize = 32 * 1024 * 1024;
const DIRECT_NODE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
enum HttpFailure {
    #[error("HTTP request timed out")]
    Timeout,
    #[error("HTTP transport failure")]
    Transport,
    #[error("HTTP response returned status {0}")]
    Status(u16),
    #[error("HTTP response exceeded the 32 MiB limit")]
    ResponseTooLarge,
    #[error("HTTP response was not valid JSON")]
    Schema,
    #[error("JSON-RPC response was not an object")]
    RpcNotObject,
    #[error("JSON-RPC response ID did not match the request")]
    RpcIdMismatch,
    #[error("JSON-RPC response contained an error")]
    RpcError,
    #[error("JSON-RPC response did not contain a result")]
    RpcMissingResult,
}

/// Sends one API request without retries and decodes a bounded JSON response.
///
/// A missing body is a GET. A present JSON body is a POST. Error messages name
/// only the transport/status/JSON category and never include response bodies.
pub async fn request_json<T: DeserializeOwned>(
    client: &Client,
    base: &str,
    path: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<T> {
    let url = join_url(base, path);
    let request = match body {
        Some(body) => client.post(url).json(body),
        None => client.get(url),
    };
    tokio::time::timeout(timeout, async move {
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::Error::from(HttpFailure::Transport))?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow::Error::from(HttpFailure::Status(status.as_u16())));
        }
        let body = read_response_body(response).await?;
        serde_json::from_slice(&body).map_err(|_| HttpFailure::Schema.into())
    })
    .await
    .map_err(|_| HttpFailure::Timeout)?
}

/// Sends one strict JSON-RPC request to a direct node endpoint without retries.
pub async fn rpc<T: DeserializeOwned>(
    client: &Client,
    endpoint: &str,
    method: &str,
    params: Value,
) -> Result<T> {
    rpc_with_timeout(client, endpoint, method, params, DIRECT_NODE_TIMEOUT).await
}

pub(crate) async fn rpc_with_timeout<T: DeserializeOwned>(
    client: &Client,
    endpoint: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<T> {
    let id = Uuid::new_v4().to_string();
    let request = json!({
        "jsonrpc": "2.0",
        "id": id.clone(),
        "method": method,
        "params": params,
    });
    let response: Value = request_json(client, endpoint, "", Some(&request), timeout).await?;
    let object = response.as_object().ok_or(HttpFailure::RpcNotObject)?;
    if object.get("id") != Some(&Value::String(id)) {
        return Err(HttpFailure::RpcIdMismatch.into());
    }
    if object.get("error").is_some_and(|error| !error.is_null()) {
        return Err(HttpFailure::RpcError.into());
    }
    let result = object
        .get("result")
        .cloned()
        .ok_or(HttpFailure::RpcMissingResult)?;
    serde_json::from_value(result).map_err(|_| HttpFailure::Schema.into())
}

pub(crate) async fn read_response_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_JSON_BODY_BYTES as u64)
    {
        return Err(HttpFailure::ResponseTooLarge.into());
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| HttpFailure::Transport)? {
        let next_len = body
            .len()
            .checked_add(chunk.len())
            .ok_or(HttpFailure::ResponseTooLarge)?;
        if next_len > MAX_JSON_BODY_BYTES {
            return Err(HttpFailure::ResponseTooLarge.into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Classifies only a failed read attempt for a caller-owned deadline loop.
///
/// This function does not retry anything itself. Call it only after the loop
/// has rechecked owned-service liveness, and never use it for Send or another
/// mutation whose outcome could be unknown after a transport deadline.
pub(crate) fn is_retryable_read_transport(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<HttpFailure>())
        .any(|failure| matches!(failure, HttpFailure::Transport | HttpFailure::Timeout))
}

/// Classifies only read-only startup probes that may remain pending.
pub(crate) fn is_startup_pending(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<HttpFailure>())
        .any(|failure| {
            matches!(
                failure,
                HttpFailure::Transport | HttpFailure::Timeout | HttpFailure::Status(502 | 503)
            )
        })
}

fn join_url(base: &str, path: &str) -> String {
    if path.is_empty() {
        base.trim_end_matches('/').to_owned()
    } else {
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}

pub(crate) fn safe_exit_code(code: Option<i32>) -> String {
    code.map_or_else(|| "unavailable".to_owned(), |code| code.to_string())
}

pub(crate) fn checked_u16(value: &str) -> Result<u16> {
    let port: u16 = value.parse().map_err(|_| anyhow::anyhow!("invalid port"))?;
    if port == 0 {
        bail!("invalid port");
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use anyhow::Result;
    use axum::{
        Json, Router,
        body::to_bytes,
        extract::{Request, State},
        http::{StatusCode, header::CONTENT_TYPE},
        response::{IntoResponse, Response},
    };
    use reqwest::Client;
    use serde::Deserialize;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

    use super::{is_retryable_read_transport, request_json, rpc};

    struct LocalServer {
        base: String,
        shutdown: Option<oneshot::Sender<()>>,
        task: JoinHandle<()>,
    }

    impl LocalServer {
        async fn start(router: Router) -> Result<Self> {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            let (shutdown, receiver) = oneshot::channel();
            let task = tokio::spawn(async move {
                let _ = axum::serve(listener, router)
                    .with_graceful_shutdown(async move {
                        let _ = receiver.await;
                    })
                    .await;
            });
            Ok(Self {
                base: format!("http://{address}"),
                shutdown: Some(shutdown),
                task,
            })
        }

        async fn shutdown(mut self) -> Result<()> {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            tokio::time::timeout(Duration::from_secs(5), &mut self.task).await??;
            Ok(())
        }
    }

    async fn echo_request(request: Request) -> Response {
        let method = request.method().to_string();
        let path = request.uri().path().to_owned();
        let body = match to_bytes(request.into_body(), 1024).await {
            Ok(body) => String::from_utf8_lossy(&body).into_owned(),
            Err(_) => "unavailable".to_owned(),
        };
        Json(json!({"method": method, "path": path, "body": body})).into_response()
    }

    #[derive(Clone)]
    enum RpcReply {
        Result,
        Error,
        MissingResult,
        WrongId,
    }

    async fn rpc_reply(State(reply): State<RpcReply>, Json(request): Json<Value>) -> Json<Value> {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let response = match reply {
            RpcReply::Result => json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}),
            RpcReply::Error => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": Value::Null,
                "error": {"code": -32603, "message": "fixture error"},
            }),
            RpcReply::MissingResult => json!({"jsonrpc": "2.0", "id": id}),
            RpcReply::WrongId => json!({"jsonrpc": "2.0", "id": "wrong", "result": null}),
        };
        Json(response)
    }

    async fn malformed_success() -> Response {
        (
            StatusCode::OK,
            [(CONTENT_TYPE, "application/json")],
            "this is not JSON",
        )
            .into_response()
    }

    async fn missing_required_field() -> Json<Value> {
        Json(json!({"other": "field"}))
    }

    async fn secret_status_body() -> Response {
        (
            StatusCode::BAD_GATEWAY,
            "UNIQUE_TEST_SECRET_MARKER_MUST_NOT_ESCAPE",
        )
            .into_response()
    }

    async fn delayed_response() -> Json<Value> {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Json(json!({"ok": true}))
    }

    async fn rejected_mutation(State(count): State<Arc<AtomicUsize>>) -> Response {
        count.fetch_add(1, Ordering::SeqCst);
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    }

    #[tokio::test]
    async fn request_json_uses_get_without_a_body_and_post_with_json() -> Result<()> {
        let server = LocalServer::start(Router::new().fallback(echo_request)).await?;
        let client = Client::new();

        let read: Value =
            request_json(&client, &server.base, "/read", None, Duration::from_secs(1)).await?;
        assert_eq!(read["method"], "GET");
        assert_eq!(read["path"], "/read");
        assert_eq!(read["body"], "");

        let write: Value = request_json(
            &client,
            &server.base,
            "/write",
            Some(&json!({"operation": "send"})),
            Duration::from_secs(1),
        )
        .await?;
        assert_eq!(write["method"], "POST");
        assert_eq!(write["path"], "/write");
        assert_eq!(write["body"], "{\"operation\":\"send\"}");
        server.shutdown().await
    }

    #[tokio::test]
    async fn rpc_validates_result_id_error_and_missing_result() -> Result<()> {
        let client = Client::new();
        let result_server = LocalServer::start(
            Router::new()
                .fallback(rpc_reply)
                .with_state(RpcReply::Result),
        )
        .await?;
        let result: Value =
            rpc(&client, &result_server.base, "getblockchaininfo", json!([])).await?;
        assert_eq!(result, json!({"ok": true}));
        result_server.shutdown().await?;

        for reply in [RpcReply::Error, RpcReply::MissingResult, RpcReply::WrongId] {
            let server =
                LocalServer::start(Router::new().fallback(rpc_reply).with_state(reply)).await?;
            assert!(
                rpc::<Value>(&client, &server.base, "getblockchaininfo", json!([]))
                    .await
                    .is_err()
            );
            server.shutdown().await?;
        }
        Ok(())
    }

    #[derive(Debug, Deserialize)]
    struct RequiredResponse {
        #[serde(rename = "required")]
        _required: String,
    }

    #[tokio::test]
    async fn malformed_and_non_success_responses_are_safe_and_not_retryable() -> Result<()> {
        let client = Client::new();
        let malformed = LocalServer::start(Router::new().fallback(malformed_success)).await?;
        let error =
            request_json::<Value>(&client, &malformed.base, "", None, Duration::from_secs(1))
                .await
                .expect_err("malformed success JSON must fail");
        assert!(!is_retryable_read_transport(&error));
        malformed.shutdown().await?;

        let missing = LocalServer::start(Router::new().fallback(missing_required_field)).await?;
        let error = request_json::<RequiredResponse>(
            &client,
            &missing.base,
            "",
            None,
            Duration::from_secs(1),
        )
        .await
        .expect_err("missing required response field must fail");
        assert!(!is_retryable_read_transport(&error));
        assert!(!is_retryable_read_transport(&anyhow::anyhow!(
            "fixture assertion failure"
        )));
        missing.shutdown().await?;

        let unsafe_body = LocalServer::start(Router::new().fallback(secret_status_body)).await?;
        let error =
            request_json::<Value>(&client, &unsafe_body.base, "", None, Duration::from_secs(1))
                .await
                .expect_err("non-success response must fail");
        assert!(
            !error
                .to_string()
                .contains("UNIQUE_TEST_SECRET_MARKER_MUST_NOT_ESCAPE")
        );
        assert!(!is_retryable_read_transport(&error));
        unsafe_body.shutdown().await?;

        let mutations = Arc::new(AtomicUsize::new(0));
        let mutation_server = LocalServer::start(
            Router::new()
                .fallback(rejected_mutation)
                .with_state(Arc::clone(&mutations)),
        )
        .await?;
        assert!(
            request_json::<Value>(
                &client,
                &mutation_server.base,
                "/api/v1/send",
                Some(&json!({"request": "mutation"})),
                Duration::from_secs(1),
            )
            .await
            .is_err()
        );
        assert_eq!(mutations.load(Ordering::SeqCst), 1);
        mutation_server.shutdown().await
    }

    #[tokio::test]
    async fn complete_read_timeout_is_retryable_only_by_a_read_deadline_loop() -> Result<()> {
        let server = LocalServer::start(Router::new().fallback(delayed_response)).await?;
        let client = Client::new();
        let error =
            request_json::<Value>(&client, &server.base, "", None, Duration::from_millis(1))
                .await
                .expect_err("deadline exhaustion must fail");
        assert_eq!(error.to_string(), "HTTP request timed out");
        assert!(is_retryable_read_transport(&error));
        server.shutdown().await
    }
}
