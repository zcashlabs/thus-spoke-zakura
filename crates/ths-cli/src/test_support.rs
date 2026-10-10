//! Finite subprocess and loopback HTTP fixtures for launcher lifecycle tests.
//!
//! Helpers are the current test executable selected with an exact test name. Without the
//! fixture environment variables that test returns immediately and does no work.

use std::{
    fs,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crate::lifecycle::LifecyclePolicy;

pub(crate) fn short_policy() -> LifecyclePolicy {
    LifecyclePolicy {
        readiness: Duration::from_millis(300),
        http_attempt: Duration::from_millis(80),
        readiness_docker: Duration::from_millis(80),
        startup_docker: Duration::from_millis(200),
        initialization: Duration::from_millis(300),
        cleanup: Duration::from_millis(400),
        poll: Duration::from_millis(5),
        termination_reserve: Duration::from_millis(40),
        ..LifecyclePolicy::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HelperMode {
    Hang,
    FloodBoth,
    Nonzero,
    HoldPipe,
}

pub(crate) struct HelperFixture {
    dir: tempfile::TempDir,
    mode: HelperMode,
}

impl HelperFixture {
    pub(crate) fn new(mode: HelperMode) -> Self {
        Self {
            dir: tempfile::tempdir().expect("helper fixture directory"),
            mode,
        }
    }

    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--exact", "test_support::helper_process"])
            .env("THS_HELPER_MODE", helper_mode_name(self.mode))
            .env("THS_HELPER_DIR", self.dir.path());
        command
    }

    pub(crate) fn wait_active(&self, timeout: Duration) {
        wait_for_file(&self.dir.path().join("active"), timeout, "helper");
    }

    pub(crate) fn grandchild_holding(&self) -> bool {
        self.dir.path().join("grandchild-alive").exists()
            && !self.dir.path().join("grandchild-exit").exists()
    }

    pub(crate) fn finish(&mut self) {
        let _ = fs::write(self.dir.path().join("finish"), b"1");
        if self.mode == HelperMode::HoldPipe {
            wait_for_file(
                &self.dir.path().join("grandchild-exit"),
                Duration::from_secs(2),
                "grandchild",
            );
        }
    }
}

fn helper_mode_name(mode: HelperMode) -> &'static str {
    match mode {
        HelperMode::Hang => "hang",
        HelperMode::FloodBoth => "flood",
        HelperMode::Nonzero => "nonzero",
        HelperMode::HoldPipe => "hold",
    }
}

fn wait_for_file(path: &std::path::Path, timeout: Duration, what: &str) {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "{what} did not finish within {timeout:?}; missing {}",
        path.display()
    );
}

#[derive(Clone, Debug)]
pub(crate) enum HttpMode {
    StallConnect,
    StallHeaders,
    StallBody,
    DribbleBody,
    Health(u16),
    Rpc(String),
}

pub(crate) struct HttpFixture {
    url: String,
    cancel: Arc<AtomicBool>,
    sockets: Arc<Mutex<Vec<TcpStream>>>,
    active: Arc<AtomicBool>,
    request: Arc<Mutex<String>>,
    requests: Arc<AtomicUsize>,
    worker: Option<thread::JoinHandle<()>>,
}

impl HttpFixture {
    pub(crate) fn new(mode: HttpMode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback fixture bind");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let port = listener.local_addr().expect("fixture port").port();
        let url = match mode {
            HttpMode::StallConnect => format!("https://127.0.0.1:{port}"),
            _ => format!("http://127.0.0.1:{port}"),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let sockets = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicBool::new(false));
        let request = Arc::new(Mutex::new(String::new()));
        let requests = Arc::new(AtomicUsize::new(0));
        let worker_cancel = Arc::clone(&cancel);
        let worker_sockets = Arc::clone(&sockets);
        let worker_active = Arc::clone(&active);
        let worker_request = Arc::clone(&request);
        let worker_requests = Arc::clone(&requests);
        let worker = thread::spawn(move || {
            http_loop(
                listener,
                mode,
                worker_cancel,
                worker_sockets,
                worker_active,
                worker_request,
                worker_requests,
            );
        });
        Self {
            url,
            cancel,
            sockets,
            active,
            request,
            requests,
            worker: Some(worker),
        }
    }

    pub(crate) fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    pub(crate) fn url(&self) -> String {
        self.url.clone()
    }

    pub(crate) fn request_body(&self) -> String {
        self.request.lock().expect("request body").clone()
    }

    pub(crate) fn wait_active(&self, timeout: Duration) {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if self.active.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("HTTP fixture did not become active within {timeout:?}");
    }

    pub(crate) fn finish(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Ok(sockets) = self.sockets.lock() {
            for socket in sockets.iter() {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
        let Some(worker) = self.worker.take() else {
            return;
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = worker.join();
            let _ = sender.send(());
        });
        if receiver.recv_timeout(Duration::from_secs(2)).is_err() {
            panic!("HTTP fixture worker did not stop within 2s");
        }
    }
}

fn http_loop(
    listener: TcpListener,
    mode: HttpMode,
    cancel: Arc<AtomicBool>,
    sockets: Arc<Mutex<Vec<TcpStream>>>,
    active: Arc<AtomicBool>,
    request: Arc<Mutex<String>>,
    requests: Arc<AtomicUsize>,
) {
    while !cancel.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(true);
                if let Ok(mut guard) = sockets.lock()
                    && let Ok(cloned) = stream.try_clone()
                {
                    guard.push(cloned);
                }
                requests.fetch_add(1, Ordering::SeqCst);
                if let Err(error) = handle_connection(&stream, &mode, &cancel, &active, &request)
                    && error.kind() != io::ErrorKind::Interrupted
                    && error.kind() != io::ErrorKind::UnexpectedEof
                {
                    eprintln!("HTTP fixture connection ended: {error}");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) if cancel.load(Ordering::SeqCst) => {
                eprintln!("HTTP fixture accept stopped: {error}");
                break;
            }
            Err(error) => {
                if cancel.load(Ordering::SeqCst) {
                    break;
                }
                panic!("HTTP fixture accept failed: {error}");
            }
        }
    }
}

fn handle_connection(
    stream: &TcpStream,
    mode: &HttpMode,
    cancel: &AtomicBool,
    active: &AtomicBool,
    request: &Mutex<String>,
) -> io::Result<()> {
    let mut stream = stream.try_clone()?;
    match mode {
        HttpMode::StallConnect => {
            let _ = read_some(&mut stream, cancel, 1)?;
            active.store(true, Ordering::SeqCst);
            stall(cancel);
            Ok(())
        }
        HttpMode::StallHeaders => {
            let _ = read_some(&mut stream, cancel, 1)?;
            active.store(true, Ordering::SeqCst);
            stall(cancel);
            Ok(())
        }
        HttpMode::StallBody => {
            let body = read_http_request(&mut stream, cancel)?;
            remember_request(request, &body);
            active.store(true, Ordering::SeqCst);
            write_all_finite(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n",
                cancel,
            )?;
            stall(cancel);
            Ok(())
        }
        HttpMode::DribbleBody => {
            let body = read_http_request(&mut stream, cancel)?;
            remember_request(request, &body);
            active.store(true, Ordering::SeqCst);
            write_all_finite(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n{",
                cancel,
            )?;
            while !cancel.load(Ordering::SeqCst) {
                if write_all_finite(&mut stream, b"x", cancel).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(30));
            }
            Ok(())
        }
        HttpMode::Health(status) => {
            let body = read_http_request(&mut stream, cancel)?;
            remember_request(request, &body);
            active.store(true, Ordering::SeqCst);
            let payload = if *status == 503 { "no" } else { "" };
            let mut headers = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                payload.len()
            );
            if *status == 302 {
                headers.push_str("Location: http://127.0.0.1:9/nowhere\r\n");
            }
            headers.push_str("\r\n");
            write_all_finite(&mut stream, headers.as_bytes(), cancel)?;
            write_all_finite(&mut stream, payload.as_bytes(), cancel)?;
            Ok(())
        }
        HttpMode::Rpc(payload) => {
            let body = read_http_request(&mut stream, cancel)?;
            remember_request(request, &body);
            active.store(true, Ordering::SeqCst);
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            write_all_finite(&mut stream, headers.as_bytes(), cancel)?;
            write_all_finite(&mut stream, payload.as_bytes(), cancel)?;
            Ok(())
        }
    }
}

fn remember_request(slot: &Mutex<String>, raw: &[u8]) {
    let text = String::from_utf8_lossy(raw);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
    *slot.lock().expect("request slot") = body;
}

fn stall(cancel: &AtomicBool) {
    while !cancel.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(5));
    }
}

fn read_some(stream: &mut TcpStream, cancel: &AtomicBool, minimum: usize) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let started = Instant::now();
    while buf.len() < minimum {
        if cancel.load(Ordering::SeqCst) || started.elapsed() > Duration::from_secs(2) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "fixture read stopped",
            ));
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "fixture peer closed",
                ));
            }
            Ok(count) => buf.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(buf)
}

fn read_http_request(stream: &mut TcpStream, cancel: &AtomicBool) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let started = Instant::now();
    loop {
        if cancel.load(Ordering::SeqCst) || started.elapsed() > Duration::from_secs(2) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "fixture read stopped",
            ));
        }
        if let Some(end) = header_end(&buf) {
            let length = content_length(&buf).unwrap_or(0);
            if buf.len() >= end + length {
                return Ok(buf);
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(buf),
            Ok(count) => buf.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn content_length(buf: &[u8]) -> Option<usize> {
    let end = header_end(buf)?;
    let headers = String::from_utf8_lossy(&buf[..end]);
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

fn write_all_finite(
    stream: &mut TcpStream,
    mut bytes: &[u8],
    cancel: &AtomicBool,
) -> io::Result<()> {
    let started = Instant::now();
    while !bytes.is_empty() {
        if cancel.load(Ordering::SeqCst) || started.elapsed() > Duration::from_secs(2) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "fixture write stopped",
            ));
        }
        match stream.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "fixture write stalled",
                ));
            }
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[test]
fn helper_process() {
    let Ok(mode) = std::env::var("THS_HELPER_MODE") else {
        return;
    };
    let dir = PathBuf::from(std::env::var("THS_HELPER_DIR").expect("helper directory"));
    if std::env::var_os("THS_HELPER_GRANDCHILD").is_some() {
        fs::write(dir.join("grandchild-alive"), b"1").expect("grandchild marker");
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) && !dir.join("finish").exists() {
            thread::sleep(Duration::from_millis(5));
        }
        let _ = std::io::stdout().write_all(b"held");
        let _ = std::io::stdout().flush();
        fs::write(dir.join("grandchild-exit"), b"1").expect("grandchild exit");
        return;
    }
    fs::write(dir.join("active"), b"1").expect("active marker");
    match mode.as_str() {
        "hang" => {
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(30) && !dir.join("finish").exists() {
                thread::sleep(Duration::from_millis(20));
            }
        }
        "flood" => {
            let chunk = vec![b'x'; 8192];
            for _ in 0..(2 * 1024 * 1024 / 8192) {
                std::io::stdout().write_all(&chunk).expect("stdout flood");
            }
            for _ in 0..(128 * 1024 / 8192) {
                std::io::stderr().write_all(&chunk).expect("stderr flood");
            }
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
        }
        "nonzero" => {
            std::io::stdout().write_all(b"out").expect("stdout");
            std::io::stderr().write_all(b"err").expect("stderr");
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            std::process::exit(7);
        }
        "hold" => {
            let child = Command::new(std::env::current_exe().expect("test executable"))
                .args(["--exact", "test_support::helper_process"])
                .env("THS_HELPER_MODE", "hold")
                .env("THS_HELPER_DIR", &dir)
                .env("THS_HELPER_GRANDCHILD", "1")
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .expect("grandchild");
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(2)
                && !dir.join("grandchild-alive").exists()
            {
                thread::sleep(Duration::from_millis(5));
            }
            fs::write(dir.join("grandchild-pid"), child.id().to_string()).expect("pid");
            std::mem::forget(child);
        }
        other => panic!("unknown helper mode {other}"),
    }
}
