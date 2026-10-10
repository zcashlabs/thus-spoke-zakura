use std::{
    sync::{Arc, Condvar, Mutex, mpsc::Receiver},
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, StatusCode,
        header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::Response,
};
use reqwest::{Client, redirect::Policy};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

use super::{MAX_JSON_BODY_BYTES, read_response_body};

const GENERATE_TIMEOUT: Duration = Duration::from_secs(120);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenerateCounts {
    pub rejected: usize,
    pub forwarded: usize,
}

#[derive(Default)]
struct ProxyState {
    armed: bool,
    rejected: usize,
    forwarded: usize,
    hidden_transaction: Option<String>,
    hidden_lookups: usize,
}

struct SharedProxy {
    upstream: String,
    client: Client,
    state: Mutex<ProxyState>,
}

#[derive(Default)]
struct FallbackReaperState {
    task: Option<JoinHandle<Result<()>>>,
    stop: bool,
}

/// Holds a proxy server JoinHandle after an unexpected owner drop.
///
/// The reaper is started before the listener. Its independent runtime keeps
/// the JoinHandle owned until it can observe graceful completion or abort and
/// join the server task, while the dropping owner waits only a bounded time.
pub(super) struct FallbackTaskReaper {
    state: Arc<(Mutex<FallbackReaperState>, Condvar)>,
    completion: Mutex<Receiver<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FallbackTaskReaper {
    pub(super) fn start() -> Result<Self> {
        let state = Arc::new((Mutex::new(FallbackReaperState::default()), Condvar::new()));
        let worker_state = Arc::clone(&state);
        let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(1);
        let (completion_sender, completion) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("ths-recovery-proxy-reaper".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                {
                    Ok(runtime) => {
                        let _ = ready_sender.send(true);
                        runtime
                    }
                    Err(_) => {
                        let _ = ready_sender.send(false);
                        return;
                    }
                };
                let task = {
                    let (lock, wake) = &*worker_state;
                    let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    loop {
                        if let Some(task) = state.task.take() {
                            break task;
                        }
                        if state.stop {
                            return;
                        }
                        state = wake
                            .wait(state)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                };
                runtime.block_on(reap_fallback_task(task));
                let _ = completion_sender.send(());
            })
            .map_err(|_| anyhow::anyhow!("creating fixture RPC-proxy reaper"))?;

        match ready_receiver.recv_timeout(SHUTDOWN_TIMEOUT) {
            Ok(true) => Ok(Self {
                state,
                completion: Mutex::new(completion),
                thread: Some(thread),
            }),
            Ok(false) | Err(_) => Err(anyhow::anyhow!("starting fixture RPC-proxy reaper runtime")),
        }
    }

    pub(super) fn hand_off(&self, task: JoinHandle<Result<()>>) {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(
            state.task.is_none(),
            "fixture reaper already owns a proxy task"
        );
        state.task = Some(task);
        wake.notify_one();
    }

    pub(super) fn wait_for_reap(&self) -> bool {
        self.completion
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .recv_timeout(SHUTDOWN_TIMEOUT)
            .is_ok()
    }

    fn request_stop(&self) {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.task.is_none() {
            state.stop = true;
            wake.notify_one();
        }
    }

    pub(super) fn stop(&mut self) {
        self.request_stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for FallbackTaskReaper {
    fn drop(&mut self) {
        self.request_stop();
    }
}

async fn reap_fallback_task(mut task: JoinHandle<Result<()>>) {
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

enum UpstreamFailure {
    Transport,
    Response,
}

/// A local, per-fixture JSON-RPC proxy that can reject one `generate([1])`.
pub struct GenerateFaultProxy {
    url: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<()>>>,
    fallback_reaper: FallbackTaskReaper,
    shared: Arc<SharedProxy>,
}

impl GenerateFaultProxy {
    pub fn hide_transaction(&self, txid: Option<String>) -> Result<()> {
        self.shared
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture proxy lock"))?
            .hidden_transaction = txid;
        Ok(())
    }

    pub fn hidden_lookups(&self) -> Result<usize> {
        Ok(self
            .shared
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture proxy lock"))?
            .hidden_lookups)
    }
    pub async fn start(upstream: String) -> Result<Self> {
        let fallback_reaper = FallbackTaskReaper::start()?;
        let client = Client::builder()
            .redirect(Policy::none())
            .build()
            .context("creating the fixture RPC-proxy client")?;
        let shared = Arc::new(SharedProxy {
            upstream,
            client,
            state: Mutex::new(ProxyState::default()),
        });
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("binding the fixture RPC proxy")?;
        let address = listener
            .local_addr()
            .context("reading the fixture RPC-proxy listener address")?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let app = Router::new()
            .fallback(proxy_request)
            .with_state(Arc::clone(&shared));
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .context("running the fixture RPC proxy")
        });

        Ok(Self {
            url: format!("http://{address}"),
            shutdown: Some(shutdown_tx),
            task: Some(task),
            fallback_reaper,
            shared,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn fail_next_generate(&self) -> Result<()> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture RPC-proxy state is unavailable"))?;
        if state.armed {
            anyhow::bail!("fixture generate fault is already armed");
        }
        state.armed = true;
        Ok(())
    }

    pub fn counts(&self) -> GenerateCounts {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        GenerateCounts {
            rejected: state.rejected,
            forwarded: state.forwarded,
        }
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let result = match self.task.as_mut() {
            Some(task) => match tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut *task).await {
                Ok(Ok(Ok(()))) => Ok(()),
                Ok(Ok(Err(_))) => Err(anyhow::anyhow!(
                    "fixture RPC proxy stopped with an internal error"
                )),
                Ok(Err(_)) => Err(anyhow::anyhow!(
                    "fixture RPC-proxy task could not be joined"
                )),
                Err(_) => {
                    task.abort();
                    let _ = (&mut *task).await;
                    Err(anyhow::anyhow!(
                        "fixture RPC proxy did not stop before its deadline"
                    ))
                }
            },
            None => Ok(()),
        };
        // The task is terminal after every completed branch above. If this
        // future is cancelled at either await, the handle remains in self.task
        // for Drop to transfer to the reaper instead of detaching it.
        self.task = None;
        self.fallback_reaper.stop();
        result
    }

    pub(crate) fn fallback_shutdown(&mut self) -> bool {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let Some(task) = self.task.take() else {
            self.fallback_reaper.request_stop();
            return true;
        };
        self.fallback_reaper.hand_off(task);
        self.fallback_reaper.wait_for_reap()
    }
}

impl Drop for GenerateFaultProxy {
    fn drop(&mut self) {
        if !self.fallback_shutdown() {
            eprintln!("ACTIVITY_RECOVERY_PROXY_CLEANUP_UNJOINED");
        }
    }
}

async fn proxy_request(State(shared): State<Arc<SharedProxy>>, request: Request) -> Response {
    if request.method() != axum::http::Method::POST {
        return json_rpc_error(
            StatusCode::METHOD_NOT_ALLOWED,
            Value::Null,
            -32600,
            "JSON-RPC proxy accepts POST requests only",
        );
    }

    let original = match to_bytes(request.into_body(), MAX_JSON_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            return json_rpc_error(
                StatusCode::BAD_REQUEST,
                Value::Null,
                -32600,
                "JSON-RPC request body exceeded the fixture limit",
            );
        }
    };
    let payload: Value = match serde_json::from_slice::<Value>(&original) {
        Ok(payload) if payload.is_object() => payload,
        _ => {
            return json_rpc_error(
                StatusCode::BAD_REQUEST,
                Value::Null,
                -32600,
                "JSON-RPC request body was invalid",
            );
        }
    };
    let id = payload.get("id").cloned().unwrap_or(Value::Null);
    let method = payload.get("method").and_then(Value::as_str);
    let is_generate = method == Some("generate");
    let is_fault_target = is_generate && payload.get("params") == Some(&json!([1]));

    let hidden = {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let hidden = method == Some("getrawtransaction")
            && state
                .hidden_transaction
                .as_deref()
                .is_some_and(|txid| payload["params"][0].as_str() == Some(txid));
        if hidden {
            state.hidden_lookups += 1;
        }
        hidden
    };
    if hidden {
        return json_rpc_error(StatusCode::OK, id, -5, "injected missing transaction");
    }

    let rejected = {
        let mut state = match shared.state.lock() {
            Ok(state) => state,
            Err(_) => {
                return json_rpc_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    id,
                    -32603,
                    "fixture RPC-proxy state is unavailable",
                );
            }
        };
        if is_fault_target && state.armed {
            state.armed = false;
            state.rejected += 1;
            true
        } else {
            false
        }
    };
    if rejected {
        return json_rpc_error(StatusCode::OK, id, -32603, "injected auto-mine failure");
    }

    if is_generate {
        let mut state = match shared.state.lock() {
            Ok(state) => state,
            Err(_) => {
                return json_rpc_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    id,
                    -32603,
                    "fixture RPC-proxy state is unavailable",
                );
            }
        };
        state.forwarded += 1;
    }

    let timeout = if is_generate {
        GENERATE_TIMEOUT
    } else {
        READ_TIMEOUT
    };
    let upstream = tokio::time::timeout(timeout, async move {
        let response = shared
            .client
            .post(&shared.upstream)
            .header(CONTENT_TYPE, "application/json")
            .body(original)
            .send()
            .await
            .map_err(|_| UpstreamFailure::Transport)?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = read_response_body(response)
            .await
            .map_err(|_| UpstreamFailure::Response)?;
        Ok::<_, UpstreamFailure>((status, headers, body))
    })
    .await;
    match upstream {
        Ok(Ok((status, headers, body))) => forwarded_response(status, &headers, body),
        Ok(Err(UpstreamFailure::Transport)) => json_rpc_error(
            StatusCode::BAD_GATEWAY,
            id,
            -32000,
            "fixture upstream transport failure",
        ),
        Ok(Err(UpstreamFailure::Response)) => json_rpc_error(
            StatusCode::BAD_GATEWAY,
            id,
            -32000,
            "fixture upstream response was unavailable",
        ),
        Err(_) => json_rpc_error(
            StatusCode::GATEWAY_TIMEOUT,
            id,
            -32000,
            "fixture upstream request timed out",
        ),
    }
}

fn json_rpc_error(status: StatusCode, id: Value, code: i64, message: &'static str) -> Response {
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": Value::Null,
        "error": { "code": code, "message": message },
    }))
    .expect("fixed JSON-RPC error payload serializes");
    let body_length = body.len();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&body_length.to_string())
            .expect("JSON-RPC error length is a valid HTTP header"),
    );
    response
}

fn forwarded_response(status: StatusCode, headers: &HeaderMap, body: Vec<u8>) -> Response {
    let connection_tokens = connection_tokens(headers);
    let body_length = body.len();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    for (name, value) in headers {
        if connection_tokens
            .as_ref()
            .is_some_and(|tokens| is_safe_response_header(name.as_str(), tokens))
        {
            response.headers_mut().append(name.clone(), value.clone());
        }
    }
    response.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&body_length.to_string())
            .expect("bounded response length is a valid HTTP header"),
    );
    response
}

fn connection_tokens(headers: &HeaderMap) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    for value in headers.get_all(CONNECTION).iter() {
        let value = value.to_str().ok()?;
        for token in value.split(',') {
            let token = HeaderName::from_bytes(token.trim().as_bytes()).ok()?;
            tokens.push(token.as_str().to_owned());
        }
    }
    Some(tokens)
}

fn is_safe_response_header(name: &str, connection_tokens: &[String]) -> bool {
    !connection_tokens.iter().any(|token| token == name)
        && !matches!(
            name,
            "connection"
                | "content-length"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        )
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use anyhow::Result;
    use axum::{
        Json, Router,
        extract::State,
        http::StatusCode,
        response::{IntoResponse, Response},
    };
    use reqwest::Client;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

    use super::{GenerateCounts, GenerateFaultProxy};

    struct LocalServer {
        url: String,
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
                url: format!("http://{address}"),
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

    async fn record_request(
        State(requests): State<Arc<Mutex<Vec<Value>>>>,
        Json(request): Json<Value>,
    ) -> Json<Value> {
        requests.lock().unwrap().push(request.clone());
        Json(json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "result": {"forwarded": true},
        }))
    }

    async fn upstream_http_error(Json(request): Json<Value>) -> Response {
        (
            StatusCode::IM_A_TEAPOT,
            [("x-fixture-error", "preserved")],
            Json(json!({
                "jsonrpc": "2.0",
                "id": request.get("id").cloned().unwrap_or(Value::Null),
                "error": {"code": -32011, "message": "upstream HTTP failure"},
            })),
        )
            .into_response()
    }

    async fn upstream_json_rpc_error(Json(request): Json<Value>) -> Json<Value> {
        Json(json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "result": Value::Null,
            "error": {"code": -32012, "message": "upstream JSON-RPC failure"},
        }))
    }

    fn request(id: &str, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    async fn post_json(client: &Client, url: &str, request: &Value) -> Result<(StatusCode, Value)> {
        let response = client.post(url).json(request).send().await?;
        let status = response.status();
        Ok((status, response.json().await?))
    }

    #[tokio::test]
    async fn proxy_forwards_unarmed_requests_and_consumes_only_matching_generate_once() -> Result<()>
    {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let upstream = LocalServer::start(
            Router::new()
                .fallback(record_request)
                .with_state(Arc::clone(&requests)),
        )
        .await?;
        let mut proxy = GenerateFaultProxy::start(upstream.url.clone()).await?;
        let client = Client::new();

        let unarmed = request("read-one", "getblockchaininfo", json!([]));
        let (status, response) = post_json(&client, proxy.url(), &unarmed).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["id"], "read-one");
        assert_eq!(response["result"], json!({"forwarded": true}));

        proxy.fail_next_generate()?;
        assert!(proxy.fail_next_generate().is_err());

        let read_after_arming = request("read-two", "getrawmempool", json!([]));
        let (status, _) = post_json(&client, proxy.url(), &read_after_arming).await?;
        assert_eq!(status, StatusCode::OK);
        let other_generate = request("generate-two", "generate", json!([2]));
        let (status, _) = post_json(&client, proxy.url(), &other_generate).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            proxy.counts(),
            GenerateCounts {
                rejected: 0,
                forwarded: 1,
            }
        );

        let matching_generate = request("generate-one", "generate", json!([1]));
        let (status, response) = post_json(&client, proxy.url(), &matching_generate).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["id"], "generate-one");
        assert_eq!(response["result"], Value::Null);
        assert_eq!(response["error"]["code"], -32603);
        assert_eq!(
            proxy.counts(),
            GenerateCounts {
                rejected: 1,
                forwarded: 1,
            }
        );

        let (status, response) = post_json(&client, proxy.url(), &matching_generate).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["result"], json!({"forwarded": true}));
        assert_eq!(
            proxy.counts(),
            GenerateCounts {
                rejected: 1,
                forwarded: 2,
            }
        );
        assert_eq!(
            requests.lock().unwrap().as_slice(),
            &[
                unarmed,
                read_after_arming,
                other_generate,
                matching_generate
            ]
        );

        proxy.shutdown().await?;
        upstream.shutdown().await
    }

    #[tokio::test]
    async fn separate_proxy_instances_do_not_consume_each_others_faults() -> Result<()> {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let upstream = LocalServer::start(
            Router::new()
                .fallback(record_request)
                .with_state(Arc::clone(&requests)),
        )
        .await?;
        let mut first = GenerateFaultProxy::start(upstream.url.clone()).await?;
        let mut second = GenerateFaultProxy::start(upstream.url.clone()).await?;
        let client = Client::new();
        let matching_generate = request("isolation", "generate", json!([1]));

        first.fail_next_generate()?;
        let (status, _) = post_json(&client, second.url(), &matching_generate).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            first.counts(),
            GenerateCounts {
                rejected: 0,
                forwarded: 0,
            }
        );
        assert_eq!(
            second.counts(),
            GenerateCounts {
                rejected: 0,
                forwarded: 1,
            }
        );

        let (status, response) = post_json(&client, first.url(), &matching_generate).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["error"]["code"], -32603);
        assert_eq!(
            first.counts(),
            GenerateCounts {
                rejected: 1,
                forwarded: 0,
            }
        );

        first.shutdown().await?;
        second.shutdown().await?;
        upstream.shutdown().await
    }

    #[tokio::test]
    async fn proxy_preserves_upstream_errors_and_bounds_transport_shutdown() -> Result<()> {
        let client = Client::new();
        let request = request("errors", "getblockchaininfo", json!([]));

        let http_error = LocalServer::start(Router::new().fallback(upstream_http_error)).await?;
        let mut proxy = GenerateFaultProxy::start(http_error.url.clone()).await?;
        let response = client.post(proxy.url()).json(&request).send().await?;
        assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
        assert_eq!(response.headers()["x-fixture-error"], "preserved");
        let response: Value = response.json().await?;
        assert_eq!(response["id"], "errors");
        assert_eq!(response["error"]["code"], -32011);
        tokio::time::timeout(Duration::from_secs(2), proxy.shutdown()).await??;
        http_error.shutdown().await?;

        let json_error =
            LocalServer::start(Router::new().fallback(upstream_json_rpc_error)).await?;
        let mut proxy = GenerateFaultProxy::start(json_error.url.clone()).await?;
        let (status, response) = post_json(&client, proxy.url(), &request).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["id"], "errors");
        assert_eq!(response["error"]["code"], -32012);
        proxy.shutdown().await?;
        json_error.shutdown().await?;

        let mut unavailable = GenerateFaultProxy::start("http://127.0.0.1:9".to_owned()).await?;
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            client.post(unavailable.url()).json(&request).send(),
        )
        .await??;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        tokio::time::timeout(Duration::from_secs(2), unavailable.shutdown()).await??;
        Ok(())
    }
}
