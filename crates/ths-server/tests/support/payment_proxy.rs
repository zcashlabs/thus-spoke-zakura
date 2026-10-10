use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::StatusCode,
    response::Response,
};
use reqwest::{Client, redirect::Policy};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Notify, oneshot},
    task::JoinHandle,
};

use super::rpc_proxy::FallbackTaskReaper;
use super::{MAX_JSON_BODY_BYTES, request_json};

struct ProxyTask {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<()>>>,
    reaper: FallbackTaskReaper,
}

impl ProxyTask {
    fn start(listener: TcpListener, app: Router) -> Result<Self> {
        let reaper = FallbackTaskReaper::start()?;
        let (shutdown, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .context("running payment fault proxy")
        });
        Ok(Self {
            shutdown: Some(shutdown),
            task: Some(task),
            reaper,
        })
    }

    async fn shutdown(&mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let result = if let Some(task) = self.task.as_mut() {
            match tokio::time::timeout(Duration::from_secs(5), &mut *task).await {
                Ok(result) => result.context("joining payment proxy")?,
                Err(_) => {
                    task.abort();
                    let _ = (&mut *task).await;
                    Err(anyhow::anyhow!(
                        "payment fault proxy exceeded shutdown deadline"
                    ))
                }
            }
        } else {
            Ok(())
        };
        // Keep ownership through both awaits so cancellation cannot detach a task.
        self.task = None;
        self.reaper.stop();
        result
    }
}

impl Drop for ProxyTask {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            self.reaper.hand_off(task);
            let _ = self.reaper.wait_for_reap();
        }
    }
}

struct ReleaseWithheldResponse(Arc<Notify>);

impl Drop for ReleaseWithheldResponse {
    fn drop(&mut self) {
        // Release the handler before the proxy owner reaps it on cancellation.
        self.0.notify_one();
    }
}

#[derive(Default)]
struct BroadcastState {
    queued: bool,
    requests: Vec<Vec<u8>>,
    queued_responses: usize,
}

struct Shared {
    upstream: String,
    client: Client,
    broadcasts: Mutex<BroadcastState>,
}

/// Forwards real gRPC streams, substituting only an explicitly armed broadcast response.
pub struct QueuedBroadcastProxy {
    port: u16,
    shared: Arc<Shared>,
    server: ProxyTask,
}

impl QueuedBroadcastProxy {
    pub async fn start(upstream_port: u16) -> Result<Self> {
        let shared = Arc::new(Shared {
            upstream: format!("http://127.0.0.1:{upstream_port}"),
            client: Client::builder()
                .http2_prior_knowledge()
                .no_proxy()
                .redirect(Policy::none())
                .timeout(Duration::from_secs(120))
                .build()?,
            broadcasts: Mutex::new(BroadcastState::default()),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let app = Router::new()
            .fallback(forward_grpc)
            .with_state(Arc::clone(&shared));
        let server = ProxyTask::start(listener, app)?;
        Ok(Self {
            port,
            shared,
            server,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn queue_next_broadcast(&self) {
        self.shared.broadcasts.lock().unwrap().queued = true;
    }

    pub fn assert_identical_retry(&self) -> Result<()> {
        let state = self.shared.broadcasts.lock().unwrap();
        anyhow::ensure!(
            state.queued_responses == 1 && state.requests.len() == 2,
            "expected one original broadcast and one queued retry"
        );
        anyhow::ensure!(
            state.requests[0] == state.requests[1],
            "retry changed the signed gRPC transaction"
        );
        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        self.server.shutdown().await
    }
}

async fn forward_grpc(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, MAX_JSON_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::empty())
                .unwrap();
        }
    };
    let queued = if path.ends_with("/SendTransaction") {
        let mut state = shared.broadcasts.lock().unwrap();
        state.requests.push(bytes.to_vec());
        if state.queued {
            state.queued = false;
            state.queued_responses += 1;
            true
        } else {
            false
        }
    } else {
        false
    };
    if queued {
        // SendResponse: error_code = 1, error_message = the exact accepted queue response.
        let message = b"transaction dropped because it is already queued for download";
        let mut protobuf = vec![8, 1, 18, message.len() as u8];
        protobuf.extend_from_slice(message);
        let mut frame = vec![0];
        frame.extend_from_slice(&(protobuf.len() as u32).to_be_bytes());
        frame.extend_from_slice(&protobuf);
        return Response::builder()
            .header("content-type", "application/grpc")
            .header("grpc-status", "0")
            .body(Body::from(frame))
            .unwrap();
    }
    let mut headers = parts.headers;
    headers.remove("host");
    match shared
        .client
        .post(format!("{}{path}", shared.upstream))
        .headers(headers)
        .body(bytes)
        .send()
        .await
    {
        Ok(response) => {
            // Preserve gRPC trailers as well as data frames during forwarding.
            let response: axum::http::Response<reqwest::Body> = response.into();
            response.map(Body::new)
        }
        Err(_) => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::empty())
            .unwrap(),
    }
}

/// The upstream handler finishes, but the client disconnects before receiving its response.
pub async fn lose_payment_response(
    client: &Client,
    upstream: &str,
    path: &str,
    body: &Value,
) -> Result<Value> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = listener.local_addr()?;
    let (completed, completion) = oneshot::channel();
    let completed = Arc::new(Mutex::new(Some(completed)));
    let release = Arc::new(Notify::new());
    let upstream = upstream.to_owned();
    let path = path.to_owned();
    let proxy_client = client.clone();
    let app = Router::new().fallback({
        let release = Arc::clone(&release);
        move |request: Request| {
            let completed = Arc::clone(&completed);
            let release = Arc::clone(&release);
            let upstream = upstream.clone();
            let path = path.clone();
            let client = proxy_client.clone();
            async move {
                let result = async {
                    let bytes = to_bytes(request.into_body(), MAX_JSON_BODY_BYTES).await?;
                    let body: Value = serde_json::from_slice(&bytes)?;
                    request_json::<Value>(
                        &client,
                        &upstream,
                        &path,
                        Some(&body),
                        Duration::from_secs(120),
                    )
                    .await
                }
                .await;
                if let Some(completed) = completed.lock().unwrap().take() {
                    let _ = completed.send(result);
                }
                release.notified().await;
                StatusCode::BAD_GATEWAY
            }
        }
    });
    let mut server = ProxyTask::start(listener, app)?;
    let release_on_drop = ReleaseWithheldResponse(Arc::clone(&release));
    let result = async {
        let mut socket = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(endpoint)).await??;
        let body = serde_json::to_vec(body)?;
        let headers = format!("POST /payment HTTP/1.1\r\nHost: {endpoint}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        tokio::time::timeout(Duration::from_secs(5), async {
            socket.write_all(headers.as_bytes()).await?;
            socket.write_all(&body).await
        }).await??;
        let mut byte = [0_u8];
        let result = tokio::select! {
            response = socket.read(&mut byte) => { let _ = response; Err(anyhow::anyhow!("client unexpectedly received the withheld payment response")) },
            result = tokio::time::timeout(Duration::from_secs(120), completion) => {
                match result { Ok(Ok(result)) => result, _ => Err(anyhow::anyhow!("waiting for withheld payment response")) }
            }
        };
        // Close the actual client socket without receiving even HTTP response headers.
        drop(socket);
        result
    }.await;
    drop(release_on_drop);
    let cleanup = server.shutdown().await;
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(response), Ok(())) => Ok(response),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcash_client_backend::proto::service::{
        RawTransaction, compact_tx_streamer_client::CompactTxStreamerClient,
    };

    #[tokio::test]
    async fn queued_proxy_returns_exact_response_over_grpc() -> Result<()> {
        let mut proxy = QueuedBroadcastProxy::start(9).await?;
        let result = async {
            proxy.queue_next_broadcast();
            let mut client =
                CompactTxStreamerClient::connect(format!("http://127.0.0.1:{}", proxy.port()))
                    .await?;
            let response = client
                .send_transaction(RawTransaction {
                    data: b"signed transaction".to_vec(),
                    height: 0,
                })
                .await?
                .into_inner();
            anyhow::ensure!(
                response.error_code == 1
                    && response.error_message
                        == "transaction dropped because it is already queued for download",
                "incorrect queued gRPC response"
            );
            let state = proxy.shared.broadcasts.lock().unwrap();
            anyhow::ensure!(
                state.requests.len() == 1 && state.queued_responses == 1 && !state.queued,
                "queued fault was not consumed exactly once"
            );
            Ok(())
        }
        .await;
        let cleanup = proxy.shutdown().await;
        result.and(cleanup)
    }

    #[tokio::test]
    async fn lost_response_proxy_finishes_upstream_without_returning_to_client() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let app = Router::new()
            .fallback(|axum::Json(body): axum::Json<Value>| async move { axum::Json(body) });
        let mut server = tokio::spawn(async move { axum::serve(listener, app).await });
        let expected = serde_json::json!({"status": "broadcast", "txid": "original"});
        let result = lose_payment_response(&Client::new(), &endpoint, "/payment", &expected).await;
        server.abort();
        let _ = (&mut server).await;
        anyhow::ensure!(
            result? == expected,
            "withheld response did not come from the upstream handler"
        );
        Ok(())
    }
}
