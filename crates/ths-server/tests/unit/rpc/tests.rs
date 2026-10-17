use super::*;

#[tokio::test]
async fn call_gives_up_on_a_node_that_never_answers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc = NodeRpc::new(format!("http://{}", listener.local_addr().unwrap()));
    let err = rpc
        .call_within::<Value>(Duration::from_millis(50), "getblockcount", json!([]))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "Zakura getblockcount timed out after 50ms");
}

#[test]
fn only_rpc_not_found_means_a_transaction_is_missing() {
    let missing = anyhow::Error::new(RpcError {
        code: -5,
        message: "not found".into(),
    })
    .context("getrawtransaction failed");
    let unavailable = anyhow::Error::new(RpcError {
        code: -28,
        message: "warming up".into(),
    });

    assert!(transaction_missing(&missing));
    assert!(!transaction_missing(&unavailable));
}

#[tokio::test]
async fn explicit_availability_statuses_are_retryable_without_a_json_rpc_body() {
    for (status, body, retryable) in [
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "temporarily unavailable",
            true,
        ),
        (axum::http::StatusCode::BAD_GATEWAY, "{", true),
        (axum::http::StatusCode::GATEWAY_TIMEOUT, "{}", true),
        (axum::http::StatusCode::TOO_MANY_REQUESTS, "busy", true),
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "{", false),
        (axum::http::StatusCode::OK, "{", false),
        (
            axum::http::StatusCode::OK,
            "{\"result\":{},\"error\":null}",
            false,
        ),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rpc = NodeRpc::new(format!("http://{}", listener.local_addr().unwrap()));
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move || async move { (status, body) }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let error = rpc.chain_info().await.unwrap_err();
        assert_eq!(
            crate::bootstrap::startup_retryable(&error),
            retryable,
            "{status}: {error:#}"
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn canonical_hash_is_none_only_above_the_tip() {
    // Zakura answers `getblockhash` above its tip with the generic code -1, so the tip tells a
    // vanished height apart from any other failure.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc = NodeRpc::new(format!("http://{}", listener.local_addr().unwrap()));
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(|axum::Json(request): axum::Json<Value>| async move {
            let response = match (
                request["method"].as_str().unwrap(),
                request["params"][0].as_u64(),
            ) {
                ("getblockchaininfo", _) => {
                    json!({"result":{"chain":"regtest","blocks":5,"bestblockhash":"5".repeat(64)},"error":null})
                }
                ("getblockhash", Some(2 | 6)) => json!({
                    "result":null,
                    "error":{"code":-1,"message":"Provided index is greater than the current tip"}
                }),
                ("getblockhash", Some(height)) if height <= 5 => {
                    json!({"result":height.to_string().repeat(64),"error":null})
                }
                (method, params) => panic!("unexpected rpc {method} {params:?}"),
            };
            axum::Json(response)
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    assert_eq!(rpc.canonical_hash(4).await.unwrap(), Some("4".repeat(64)));
    assert_eq!(rpc.canonical_hash(6).await.unwrap(), None);
    let error = rpc.canonical_hash(2).await.unwrap_err();
    assert!(
        error.to_string().contains("getblockhash failed"),
        "{error:#}"
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}
