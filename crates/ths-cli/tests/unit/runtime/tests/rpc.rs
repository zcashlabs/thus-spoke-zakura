use std::{
    io::{BufRead, Read, Write},
    sync::mpsc,
    time::Duration,
};

use super::*;

#[test]
fn getblockcount_prints_the_height() {
    let (url, requests) = serve(
        "200 OK",
        r#"{"jsonrpc":"2.0","id":1,"result":42,"error":null}"#,
    );

    let output = call_rpc(&url, "getblockcount", &[], false).unwrap();

    assert_eq!(output, "42");
    let request: serde_json::Value =
        serde_json::from_str(&requests.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
    assert_eq!(request["jsonrpc"], "2.0");
    assert_eq!(request["id"], 1);
    assert_eq!(request["method"], "getblockcount");
    assert_eq!(request["params"], serde_json::json!([]));
}

#[test]
fn string_results_are_unquoted_and_raw_prints_the_envelope() {
    let (url, _requests) = serve(
        "200 OK",
        r#"{"jsonrpc":"2.0","id":1,"result":"abcd","error":null}"#,
    );
    assert_eq!(
        call_rpc(&url, "getbestblockhash", &[], false).unwrap(),
        "abcd"
    );

    let (url, _requests) = serve(
        "200 OK",
        r#"{"jsonrpc":"2.0","id":1,"result":{"blocks":2},"error":null}"#,
    );
    assert_eq!(
        call_rpc(&url, "getblockchaininfo", &[], false).unwrap(),
        "{\n  \"blocks\": 2\n}"
    );

    let (url, _requests) = serve(
        "200 OK",
        r#"{"jsonrpc":"2.0","id":1,"result":{"blocks":2},"error":null}"#,
    );
    let raw = call_rpc(&url, "getblockchaininfo", &[], true).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["result"]["blocks"], 2);
    assert_eq!(parsed["jsonrpc"], "2.0");
    assert!(raw.contains('\n'), "{raw}");
}

#[test]
fn parameters_keep_json_literals_and_leave_other_text_as_strings() {
    let (url, requests) = serve(
        "200 OK",
        r#"{"jsonrpc":"2.0","id":1,"result":null,"error":null}"#,
    );

    let output = call_rpc(
        &url,
        "getblock",
        &[
            "0000abcd".to_owned(),
            "1".to_owned(),
            "true".to_owned(),
            r#"{"verbosity":1}"#.to_owned(),
            "latest".to_owned(),
            "-1".to_owned(),
        ],
        false,
    )
    .unwrap();

    assert_eq!(output, "null");
    let request: serde_json::Value =
        serde_json::from_str(&requests.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
    assert_eq!(
        request["params"],
        serde_json::json!(["0000abcd", 1, true, {"verbosity": 1}, "latest", -1])
    );
}

#[test]
fn rpc_errors_report_the_node_message() {
    let (url, _requests) = serve(
        "200 OK",
        r#"{"jsonrpc":"2.0","id":1,"result":null,"error":{"code":-32601,"message":"Method not found"}}"#,
    );
    let error = call_rpc(&url, "missing", &[], false).unwrap_err();
    assert_eq!(error.to_string(), "Method not found");

    let (url, _requests) = serve(
        "500 Internal Server Error",
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"node is shutting down"}}"#,
    );
    let error = call_rpc(&url, "getblockcount", &[], true).unwrap_err();
    assert_eq!(error.to_string(), "node is shutting down");
}

#[test]
fn non_json_http_errors_name_the_status() {
    let (url, _requests) = serve("502 Bad Gateway", "bad gateway");
    let error = call_rpc(&url, "getblockcount", &[], false).unwrap_err();
    assert!(error.to_string().contains("502"), "{}", error.to_string());
}

#[test]
fn calls_stay_on_loopback() {
    let error = call_rpc("http://example.com", "getblockcount", &[], false).unwrap_err();
    assert!(
        error.to_string().contains("non-loopback"),
        "{}",
        error.to_string()
    );
}

#[test]
fn stop_is_refused_unless_confirmed() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let name = "default".parse().unwrap();

    for method in ["stop", "STOP", "Stop"] {
        let error = runtime.rpc(&name, method, &[], false, false).unwrap_err();
        assert_eq!(error.to_string(), STOP_REFUSAL, "{method}");
    }

    let error = runtime.rpc(&name, "stop", &[], false, true).unwrap_err();
    assert!(
        error.to_string().contains("does not exist"),
        "{}",
        error.to_string()
    );
}

#[test]
fn rpc_refuses_a_foreign_container_that_reuses_the_name() {
    let (url, requests) = serve(
        "200 OK",
        r#"{"jsonrpc":"2.0","id":1,"result":"foreign-node-was-called","error":null}"#,
    );
    let fixture = RpcFixture::new(&url, "original");
    let docker = InspectDocker::new(
        "original",
        None,
        zakura_inspect("foreign", url_port(&url), serde_json::json!({})),
    );

    let error = call(&fixture, docker, "getblockcount", false).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("refusing to adopt a replacement"),
        "{}",
        error.to_string()
    );
    assert!(requests.try_recv().is_err(), "foreign container was called");
}

#[test]
fn rpc_refuses_a_same_name_replacement() {
    let (url, requests) = serve(
        "200 OK",
        r#"{"jsonrpc":"2.0","id":1,"result":"replacement-was-called","error":null}"#,
    );
    let port = url_port(&url);
    let fixture = RpcFixture::new(&url, "original");
    let replacement = zakura_inspect(
        "replacement",
        port,
        serde_json::json!({"com.zakura.ths.instance": "alpha"}),
    );
    let docker = InspectDocker::new("original", Some(replacement.clone()), replacement);

    let error = call(&fixture, docker, "stop", true).unwrap_err();

    assert!(
        error.to_string().contains("does not match replacement"),
        "{}",
        error.to_string()
    );
    assert!(
        requests.try_recv().is_err(),
        "replacement container was called"
    );
}

#[test]
fn rpc_calls_the_recorded_owned_container() {
    let (url, requests) = serve(
        "200 OK",
        r#"{"jsonrpc":"2.0","id":1,"result":42,"error":null}"#,
    );
    let port = url_port(&url);
    let fixture = RpcFixture::new(&url, "original");
    let docker = InspectDocker::new(
        "original",
        Some(zakura_inspect(
            "original",
            port,
            serde_json::json!({"com.zakura.ths.instance": "alpha"}),
        )),
        zakura_inspect("foreign", port, serde_json::json!({})),
    );

    call(&fixture, docker, "getblockcount", false).unwrap();
    let request: serde_json::Value =
        serde_json::from_str(&requests.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
    assert_eq!(request["method"], "getblockcount");
}

#[test]
fn rpc_refuses_when_the_recorded_container_publishes_a_different_port() {
    let (url, requests) = serve(
        "200 OK",
        r#"{"jsonrpc":"2.0","id":1,"result":"wrong-port-was-called","error":null}"#,
    );
    let fixture = RpcFixture::new(&url, "original");
    let docker = InspectDocker::new(
        "original",
        Some(zakura_inspect(
            "original",
            1,
            serde_json::json!({"com.zakura.ths.instance": "alpha"}),
        )),
        String::new(),
    );

    let error = call(&fixture, docker, "getblockcount", false).unwrap_err();

    assert!(
        error.to_string().contains("publishes RPC on 127.0.0.1:1"),
        "{}",
        error.to_string()
    );
    assert!(requests.try_recv().is_err(), "stale RPC url was called");
}

fn call(fixture: &RpcFixture, docker: InspectDocker, method: &str, yes: bool) -> Result<()> {
    super::with_rpc_docker(docker, || {
        fixture.runtime.rpc(&fixture.name, method, &[], false, yes)
    })
}

fn serve(status: &'static str, body: &str) -> (String, mpsc::Receiver<String>) {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (requests, received) = mpsc::channel();
    let body = body.to_owned();
    std::thread::spawn(move || {
        // Local health probes can connect to an ephemeral port before the client does.
        loop {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            if request_line.starts_with("HEAD ") {
                continue;
            }
            let mut request = vec![0; length];
            reader.read_exact(&mut request).unwrap();
            requests.send(String::from_utf8(request).unwrap()).unwrap();
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            break;
        }
    });
    (url, received)
}

struct RpcFixture {
    _dir: tempfile::TempDir,
    runtime: Runtime,
    name: InstanceName,
}

impl RpcFixture {
    fn new(rpc: &str, container_id: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        let name = name("alpha");
        fs::create_dir_all(runtime.instance_dir(&name)).unwrap();
        let mut endpoints = endpoints_for(&host_ports(0).unwrap());
        endpoints.rpc = rpc.to_owned();
        runtime.write_instance(&name, &endpoints).unwrap();
        let mut journal =
            recovery::RecoveryJournal::create(runtime.recovery_path(&name), &name).unwrap();
        let index = journal
            .begin(
                recovery::ResourceRef {
                    kind: recovery::ResourceKind::Container,
                    name: format!("{}-zakura", prefix(&name)),
                    identity: None,
                },
                Vec::new(),
                recovery::MutationOperation::Create,
            )
            .unwrap();
        journal
            .finish(
                index,
                recovery::MutationOutcome::Acknowledged,
                Some(container_id.to_owned()),
            )
            .unwrap();
        Self {
            _dir: dir,
            runtime,
            name,
        }
    }
}

struct InspectDocker {
    recorded_id: &'static str,
    recorded: Option<String>,
    named: String,
}

impl InspectDocker {
    fn new(recorded_id: &'static str, recorded: Option<String>, named: String) -> Self {
        Self {
            recorded_id,
            recorded,
            named,
        }
    }
}

impl DockerResourceCommands for InspectDocker {
    fn output(&self, args: &[&str]) -> Result<String> {
        match args {
            ["container", "inspect", id] if *id == self.recorded_id => self
                .recorded
                .clone()
                .ok_or_else(|| anyhow!("no such container {id}")),
            ["container", "inspect", _] => Ok(self.named.clone()),
            ["container", "ls", ..] => Ok("ths-alpha-zakura\n".into()),
            _ => bail!("unexpected Docker read: {}", args.join(" ")),
        }
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        bail!("unexpected Docker run: {}", args.join(" "));
    }
}

fn zakura_inspect(id: &str, port: u16, labels: serde_json::Value) -> String {
    serde_json::json!([{
        "Id": id,
        "Name": "/ths-alpha-zakura",
        "Config": {"Labels": labels},
        "State": {"Running": true},
        "NetworkSettings": {
            "Ports": {
                "18232/tcp": [{"HostIp": "127.0.0.1", "HostPort": port.to_string()}]
            }
        }
    }])
    .to_string()
}

fn url_port(url: &str) -> u16 {
    reqwest::Url::parse(url).unwrap().port().unwrap()
}

fn name(value: &str) -> InstanceName {
    value.parse().unwrap()
}
