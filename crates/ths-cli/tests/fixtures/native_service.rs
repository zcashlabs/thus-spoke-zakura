//! Small real processes for native launcher integration tests; no Docker or Python.
use std::{
    env, fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::Command,
    thread,
    time::Duration,
};
fn argument(args: &[String], flag: &str) -> String {
    args.windows(2).find(|pair| pair[0] == flag).unwrap()[1].clone()
}
fn serve(mut stream: TcpStream, body: &str, drip: bool) {
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut buffer = [0; 8192];
    let Ok(read) = stream.read(&mut buffer) else {
        return;
    };
    let body = if buffer[..read].starts_with(b"GET /api/v1/activity")
        && !body.contains("activity-error")
    {
        "[]"
    } else {
        body
    };
    if drip {
        if stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999999\r\n\r\n")
            .is_err()
        {
            return;
        }
        loop {
            if stream.write_all(b" ").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }
    } else {
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }
}
fn main() {
    let args: Vec<String> = env::args().collect();
    let service = &args[1];
    let mode = &args[2];
    if service == "ths-server" && args[3] == "init" {
        fs::write(
            "ports",
            format!(
                "{} {}",
                argument(&args, "--native-rpc-port"),
                argument(&args, "--native-p2p-port")
            ),
        )
        .unwrap();
        if mode == "init-fail" {
            std::process::exit(7);
        }
        if mode == "init-hang" {
            fs::write("init.pid.pending", std::process::id().to_string()).unwrap();
            fs::rename("init.pid.pending", "init.pid").unwrap();
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
        if mode == "init-descendant" {
            let child = Command::new("/bin/sleep").arg("300").spawn().unwrap();
            fs::write("descendant", child.id().to_string()).unwrap();
        }
        return;
    }
    if service == "zakurad" && mode == "foreign-listener" {
        Command::new(env::current_exe().unwrap())
            .args(["foreign-node", "normal"])
            .spawn()
            .unwrap();
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    for key in ["HTTP_PROXY", "HTTPS_PROXY", "THS_BACKEND", "ZEBRA_CONFIG"] {
        assert!(env::var_os(key).is_none());
    }
    let (address, body) = match service.as_str() {
        "zakurad" | "foreign-node" => {
            let ports = fs::read_to_string("ports").unwrap();
            let ports: Vec<_> = ports.split_whitespace().collect();
            let p2p = TcpListener::bind(format!("127.0.0.1:{}", ports[1])).unwrap();
            thread::spawn(move || for _ in p2p.incoming() {});
            let genesis = if mode == "wrong-genesis" {
                "foreign"
            } else {
                "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327"
            };
            (
                format!("127.0.0.1:{}", ports[0]),
                format!("{{\"result\":\"{genesis}\",\"error\":null}}"),
            )
        }
        "lightwalletd" => (argument(&args, "--grpc-bind-addr"), "{}".into()),
        _ => {
            let name = if mode == "wrong-identity" {
                "foreign".into()
            } else {
                env::var("THS_INSTANCE").unwrap()
            };
            (
                env::var("THS_LISTEN").unwrap(),
                format!(
                    "{{\"instance\":\"{name}\",\"network\":\"Regtest\",\"settled\":true,\"node\":{{\"blocks\":1}},\"wallet_sync\":{{\"fully_scanned_height\":1}}}}"
                ),
            )
        }
    };
    let listener = TcpListener::bind(address).unwrap();
    fs::write(format!("{service}.pid"), std::process::id().to_string()).unwrap();
    if mode == "service-fail" && service == "ths-server" {
        thread::spawn(|| {
            thread::sleep(Duration::from_secs(2));
            std::process::exit(8);
        });
    }
    for stream in listener.incoming() {
        let body = body.clone();
        let drip = mode == "drip";
        thread::spawn(move || serve(stream.unwrap(), &body, drip));
    }
}
