use super::*;
use crate::api::router;
use crate::api::{
    error::{ApiError, not_found},
    explorer::NO_BLOCK,
};
use crate::state::tests::started_offline_state;
use crate::{rpc::RpcError, wallet::PaymentError};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
#[derive(Clone, Default)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs<T>(operation: impl FnOnce() -> T) -> (T, String) {
    let output = LogWriter::default();
    let writer = output.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    // Keep capture scoped to the synchronous operation so parallel tests and
    // tasks running on other threads cannot write into this test's log.
    let result = tracing::subscriber::with_default(subscriber, operation);
    let logs = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
    (result, logs)
}

#[tokio::test]
async fn api_error_logs_full_rpc_error_chain() {
    let error: RpcError =
        serde_json::from_value(json!({"code": -28, "message": "warming up"})).unwrap();
    let error = anyhow::Error::new(error)
        .context("Zakura getblock failed")
        .context("loading block details");
    let (response, logs) = capture_logs(|| ApiError::from(error).into_response());

    assert_eq!(logs.lines().count(), 1, "{logs}");
    assert!(logs.contains("ERROR"), "{logs}");
    assert!(
        logs.contains(
            "error=loading block details: Zakura getblock failed: RPC error -28: warming up"
        ),
        "{logs}"
    );
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"error": {"message": "loading block details", "status": 500}})
    );
}

#[tokio::test]
async fn api_error_logs_skip_expected_failures() {
    for (error, status) in [
        (
            anyhow::Error::new(IdempotencyConflict),
            StatusCode::CONFLICT,
        ),
        (
            anyhow::Error::new(PaymentError::InsufficientFunds {
                available: 100_000_000,
                required: 100_010_000,
            }),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            anyhow::Error::new(PaymentError::TransparentMemo),
            StatusCode::BAD_REQUEST,
        ),
        (
            anyhow::Error::new(PaymentError::TreasuryExhausted),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            anyhow::Error::new(PaymentError::ScanRequired),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        let error = error.context("payment could not complete");
        let (response, logs) = capture_logs(|| ApiError::from(error).into_response());
        assert!(logs.is_empty(), "{status}: {logs}");
        assert_eq!(response.status(), status);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"error": {"message": "payment could not complete", "status": status.as_u16()}})
        );
    }
}

#[tokio::test]
async fn api_error_logs_skip_explorer_misses() {
    for code in [-5, -8] {
        let error: RpcError =
            serde_json::from_value(json!({"code": code, "message": "not found"})).unwrap();
        let error = anyhow::Error::new(error).context("Zakura getblock failed");
        let (response, logs) = capture_logs(|| not_found(error, NO_BLOCK).into_response());
        assert!(logs.is_empty(), "{code}: {logs}");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"error": {"message": NO_BLOCK, "status": 404}})
        );
    }
}

#[test]
fn a_dry_treasury_is_reported_as_unavailable() {
    assert_eq!(
        ApiError::from(anyhow::Error::new(PaymentError::TreasuryExhausted)).status,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[test]
fn an_unaffordable_send_is_reported_as_a_client_error() {
    assert_eq!(
        ApiError::from(anyhow::Error::new(PaymentError::InsufficientFunds {
            available: 100_000_000,
            required: 100_010_000,
        }))
        .status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

/// A request that never reaches the handler must still use the JSON envelope.
/// Axum otherwise answers `Json` type errors with 422 plain text, which the
/// dashboard reports as `Request failed (422)`.
#[tokio::test]
async fn request_parsing_errors_use_the_json_error_envelope() {
    let (state, _dir) = started_offline_state();
    let cases = [
        (
            post_json(
                "/api/v1/send",
                r#"{"from_account":256,"to_account":2,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":1000,"idempotency_key":"envelope-check-2"}"#,
            ),
            "from_account must be between 1 and 5",
        ),
        (
            post_json(
                "/api/v1/faucet",
                r#"{"account_id":1,"pool":"ironwood","amount_zatoshi":-1,"idempotency_key":"envelope-check-2"}"#,
            ),
            "amount_zatoshi must be greater than zero and no more than 5 ZEC",
        ),
        (
            Request::get("/api/v1/blocks?limit=abc")
                .body(Body::empty())
                .unwrap(),
            "limit must be a whole number",
        ),
        (
            post_json("/api/v1/mine", r#"{"blocks":"Content-Type"}"#),
            "blocks must be between 1 and 10,000",
        ),
        (
            post_json("/api/v1/mine", r#"{"blocks":"valid UTF-8"}"#),
            "blocks must be between 1 and 10,000",
        ),
        (
            post_json("/api/v1/reorg", r#"{"depth":"three"}"#),
            "depth must be a whole number",
        ),
        (
            post_json("/api/v1/rollback", r#"{"depth":"three"}"#),
            "depth must be a whole number",
        ),
        (
            Request::builder()
                .uri("/api/v1/blocks/%FF")
                .body(Body::empty())
                .unwrap(),
            "path must be valid text",
        ),
        (
            post_json(
                "/api/v1/send",
                r#"{"from_account":7,"to_account":2,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":1000,"idempotency_key":"envelope-check-2"}"#,
            ),
            "account must be between 1 and 5",
        ),
    ];

    for (request, message) in cases {
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            panic!(
                "expected a JSON error envelope, got {}",
                String::from_utf8_lossy(&bytes)
            )
        });
        assert_eq!(body["error"]["status"], 400, "{body}");
        assert_eq!(body["error"]["message"], message, "{body}");
    }

    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/mine")
                .body(Body::from(r#"{"blocks":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let body = json_error_body(response).await;
    assert_eq!(body["error"]["status"], 415, "{body}");
    assert_eq!(
        body["error"]["message"], "Content-Type must be application/json",
        "{body}"
    );

    let response = router(state)
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/mine")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    let allow = response
        .headers()
        .get(header::ALLOW)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(
        allow.split(',').any(|method| method.trim() == "POST"),
        "{allow}"
    );
    let body = json_error_body(response).await;
    assert_eq!(body["error"]["status"], 405, "{body}");
    assert_eq!(body["error"]["message"], "method is not allowed", "{body}");
}

#[test]
fn parse_errors_use_the_same_language_as_handler_checks() {
    let cases = [
        (
            "Failed to deserialize the JSON body into the target type: from_account: invalid value: integer `256`, expected u8 at line 1 column 19",
            "from_account must be between 1 and 5",
        ),
        (
            "Failed to deserialize the JSON body into the target type: from_account: invalid type: string \"7\", expected u8 at line 1 column 19",
            "from_account must be between 1 and 5",
        ),
        (
            "Failed to deserialize the JSON body into the target type: from_account: invalid type: floating point `1.5`, expected u8 at line 1 column 19",
            "from_account must be between 1 and 5",
        ),
        (
            "Failed to deserialize the JSON body into the target type: from_account: invalid type: null, expected u8 at line 1 column 20",
            "from_account must be between 1 and 5",
        ),
        (
            "Failed to deserialize the JSON body into the target type: to_account: invalid value: integer `256`, expected u8 at line 1 column 34",
            "to_account must be between 1 and 5",
        ),
        (
            "Failed to deserialize the JSON body into the target type: account_id: invalid value: integer `256`, expected u8 at line 1 column 17",
            "account_id must be between 1 and 5",
        ),
        (
            "Failed to deserialize the JSON body into the target type: missing field `from_account` at line 1 column 130",
            "from_account is required",
        ),
        (
            "Failed to deserialize the JSON body into the target type: amount_zatoshi: invalid value: integer `-1`, expected u64 at line 1 column 107",
            "amount_zatoshi must be between 1 and 2100000000000000",
        ),
        (
            "Failed to deserialize the JSON body into the target type: pool: invalid type: integer `1`, expected a string at line 1 column 24",
            "pool must be transparent or ironwood",
        ),
        (
            "Failed to deserialize the JSON body into the target type: missing field `pool` at line 1 column 72",
            "pool is required",
        ),
        (
            "Failed to deserialize the JSON body into the target type: invalid type: null, expected struct SendRequest at line 1 column 4",
            "request body must be a JSON object",
        ),
        (
            "Failed to deserialize the JSON body into the target type: invalid length 0, expected struct SendRequest with 8 elements at line 1 column 2",
            "request body must be a JSON object",
        ),
        (
            "Failed to parse the request body as JSON: EOF while parsing an object at line 1 column 1",
            "request body must be valid JSON",
        ),
        (
            "Failed to parse the request body as JSON: EOF while parsing a value at line 1 column 0",
            "request body must be valid JSON",
        ),
        (
            "Failed to deserialize the JSON body into the target type: blocks: invalid type: string \"Content-Type\", expected u32 at line 1 column 28",
            "blocks must be between 1 and 10,000",
        ),
        (
            "Failed to deserialize the JSON body into the target type: blocks: invalid type: string \"valid UTF-8\", expected u32 at line 1 column 26",
            "blocks must be between 1 and 10,000",
        ),
        (
            "Failed to deserialize query string: limit: invalid digit found in string",
            "limit must be a whole number",
        ),
        (
            "Failed to deserialize query string: limit: cannot parse integer from empty string",
            "limit must be a whole number",
        ),
        (
            "Failed to deserialize query string: before: invalid digit found in string",
            "before must be a whole number",
        ),
        (
            "Failed to deserialize query string: missing field `q`",
            "q is required",
        ),
        (
            "Failed to deserialize query string: missing field `idempotency_key`",
            "idempotency_key is required",
        ),
        (
            "Failed to deserialize the JSON body into the target type: blocks: invalid type: string \"abc\", expected u32 at line 1 column 15",
            "blocks must be between 1 and 10,000",
        ),
        (
            "Failed to deserialize the JSON body into the target type: source_pool: invalid type: integer `1`, expected a string at line 1 column 40",
            "source_pool must be transparent or ironwood",
        ),
        (
            "Failed to deserialize the JSON body into the target type: destination_pool: invalid type: null, expected a string at line 1 column 60",
            "destination_pool must be transparent or ironwood",
        ),
        (
            "Failed to deserialize the JSON body into the target type: idempotency_key: invalid type: integer `1`, expected a string at line 1 column 80",
            "idempotency_key must contain 8-128 visible ASCII characters",
        ),
        (
            "Failed to deserialize the JSON body into the target type: confirmation: invalid type: integer `1`, expected a string at line 1 column 17",
            "exact confirmation phrase required",
        ),
        (
            "Failed to deserialize the JSON body into the target type: uri: invalid type: integer `1`, expected a string at line 1 column 8",
            "uri must be a valid payment URI",
        ),
        (
            "Failed to deserialize the JSON body into the target type: address: invalid type: integer `1`, expected a string at line 1 column 12",
            "address must be a unified or transparent Regtest address",
        ),
        (
            "Failed to deserialize the JSON body into the target type: to_address: invalid type: integer `1`, expected a string at line 1 column 16",
            "to_address must be a unified or transparent Regtest address",
        ),
        (
            "Failed to deserialize the JSON body into the target type: memo: invalid type: integer `1`, expected a string at line 1 column 10",
            "memo must be text",
        ),
    ];
    for (raw, expected) in cases {
        let path = if expected.starts_with("amount_zatoshi must be between") {
            "/api/v1/send"
        } else {
            ""
        };
        assert_eq!(friendly_parse_message(raw, path), expected, "{raw}");
    }
    assert_eq!(
        friendly_parse_message(
            "Failed to deserialize the JSON body into the target type: amount_zatoshi: invalid value: integer `-1`, expected u64 at line 1 column 53",
            "/api/v1/faucet",
        ),
        "amount_zatoshi must be greater than zero and no more than 5 ZEC"
    );
    assert_eq!(
        friendly_parse_message(
            "Failed to deserialize the JSON body into the target type: amount_zatoshi: invalid type: string \"1\", expected u64 at line 1 column 35",
            "/api/v1/faucet/address",
        ),
        "amount_zatoshi must be greater than zero and no more than 5 ZEC"
    );
}

async fn json_error_body(response: axum::response::Response) -> Value {
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        panic!(
            "expected a JSON error envelope, got {}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

fn post_json(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}
