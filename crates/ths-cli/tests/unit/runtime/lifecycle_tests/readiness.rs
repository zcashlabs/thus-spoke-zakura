use super::*;

#[test]
fn oversized_body_is_rejected_before_waiting_for_the_rest() {
    for probe in [Probe::Health, Probe::ZakuraTip] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (finish, finished) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 4096];
            std::io::Read::read(&mut stream, &mut request).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n123456789")
                .unwrap();
            finished.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        let policy = LifecyclePolicy {
            http_body_limit: 8,
            ..short_policy()
        };
        let failure = probe_once(
            &readiness_client().unwrap(),
            &url,
            probe,
            Deadline::after(policy.readiness),
            Cancellation::Ignore,
            &policy,
        )
        .unwrap_err();
        finish.send(()).unwrap();
        worker.join().unwrap();
        assert!(format!("{failure:#}").contains("oversized"), "{failure:#}");
    }
}

#[test]
fn shutdown_during_an_active_http_attempt_is_observed() {
    for mode in [
        HttpMode::StallConnect,
        HttpMode::StallHeaders,
        HttpMode::StallBody,
    ] {
        let mut fixture = HttpFixture::new(mode);
        let policy = short_policy();
        let (sender, shutdown) = Shutdown::channel();
        let requested = || shutdown.try_interrupted();
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                fixture.wait_active(Duration::from_secs(2));
                sender.send(Ended::Interrupted).unwrap();
            });
            assert!(
                probe_once(
                    &readiness_client().unwrap(),
                    &fixture.url(),
                    Probe::Health,
                    Deadline::after(policy.readiness),
                    Cancellation::Observe(&requested),
                    &policy,
                )
                .is_err()
            );
        });
        assert!(shutdown.try_interrupted());
        within_attempt(started, &policy);
        fixture.finish();
    }
}

#[test]
fn slow_body_cannot_restart_an_http_attempt() {
    let policy = short_policy();
    let mut fixture = HttpFixture::new(HttpMode::DribbleBody);
    let client = readiness_client().unwrap();
    let started = Instant::now();
    let failure = probe_once(
        &client,
        &fixture.url(),
        Probe::ZakuraTip,
        Deadline::after(policy.readiness),
        Cancellation::Ignore,
        &policy,
    )
    .unwrap_err();
    assert!(format!("{failure:#}").contains("tim"), "{failure:#}");
    within_attempt(started, &policy);
    assert_eq!(fixture.request_count(), 1);
    fixture.finish();
}

#[test]
fn stalled_connect_headers_and_body_stay_inside_one_attempt() {
    let policy = short_policy();
    for mode in [
        HttpMode::StallConnect,
        HttpMode::StallHeaders,
        HttpMode::StallBody,
    ] {
        let mut fixture = HttpFixture::new(mode);
        let client = readiness_client().unwrap();
        let started = Instant::now();
        let failure = probe_once(
            &client,
            &fixture.url(),
            Probe::Health,
            Deadline::after(policy.readiness),
            Cancellation::Ignore,
            &policy,
        )
        .unwrap_err();
        assert!(format!("{failure:#}").contains("tim"), "{failure:#}");
        within_attempt(started, &policy);
        fixture.wait_active(Duration::from_secs(2));
        fixture.finish();
    }
}

#[test]
fn cancellation_before_send_does_not_open_a_probe() {
    let policy = short_policy();
    let mut fixture = HttpFixture::new(HttpMode::StallHeaders);
    let client = readiness_client().unwrap();
    let requested = || true;
    let started = Instant::now();
    let failure = probe_once(
        &client,
        &fixture.url(),
        Probe::Health,
        Deadline::after(policy.readiness),
        Cancellation::Observe(&requested),
        &policy,
    )
    .unwrap_err();
    assert!(format!("{failure:#}").contains("cancelled"), "{failure:#}");
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(fixture.request_count(), 0);
    fixture.finish();
}

#[test]
fn shutdown_during_retry_uses_the_original_readiness_deadline() {
    let policy = short_policy();
    let mut fixture = HttpFixture::new(HttpMode::Health(503));
    let (sender, shutdown) = Shutdown::channel();
    let url = fixture.url();
    let started = Instant::now();
    let sender = sender;
    std::thread::spawn(move || {
        let waiting = Instant::now();
        while waiting.elapsed() < Duration::from_secs(2) && fixture.request_count() == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        sender.send(Ended::Interrupted).unwrap();
        fixture.finish();
    });
    let docker = RunningDocker;
    let failure = wait_ready(
        &url,
        "app-id",
        Deadline::after(policy.readiness),
        &shutdown,
        &policy,
        &docker,
    )
    .unwrap_err();
    assert!(failure.to_string().contains("interrupted"), "{failure:#}");
    assert!(
        started.elapsed() < policy.readiness + Duration::from_millis(500),
        "retry took {:?}",
        started.elapsed()
    );
}

#[test]
fn repeated_probes_share_one_phase_deadline() {
    let mut policy = short_policy();
    policy.readiness = Duration::from_millis(900);
    let mut fixture = HttpFixture::new(HttpMode::Health(503));
    let (_sender, shutdown) = Shutdown::channel();
    let docker = RunningDocker;
    let started = Instant::now();
    let failure = wait_ready(
        &fixture.url(),
        "app-id",
        Deadline::after(policy.readiness),
        &shutdown,
        &policy,
        &docker,
    )
    .unwrap_err();
    assert!(failure.to_string().contains("timed out"), "{failure:#}");
    assert!(
        fixture.request_count() >= 2,
        "requests={}",
        fixture.request_count()
    );
    assert!(
        started.elapsed() < policy.readiness + Duration::from_millis(500),
        "phase took {:?}",
        started.elapsed()
    );
    fixture.finish();
}

#[test]
fn health_accepts_statuses_below_400_and_rejects_503() {
    let policy = short_policy();
    let client = readiness_client().unwrap();
    for status in [204u16, 302] {
        let mut fixture = HttpFixture::new(HttpMode::Health(status));
        let ready = probe_once(
            &client,
            &fixture.url(),
            Probe::Health,
            Deadline::after(policy.readiness),
            Cancellation::Ignore,
            &policy,
        )
        .unwrap();
        assert!(ready, "status {status}");
        fixture.finish();
    }
    let mut fixture = HttpFixture::new(HttpMode::Health(503));
    let ready = probe_once(
        &client,
        &fixture.url(),
        Probe::Health,
        Deadline::after(policy.readiness),
        Cancellation::Ignore,
        &policy,
    )
    .unwrap();
    assert!(!ready);
    fixture.finish();
}

#[test]
fn zakura_tip_accepts_a_string_result_and_rejects_malformed_bodies() {
    let policy = short_policy();
    let client = readiness_client().unwrap();
    let mut fixture = HttpFixture::new(HttpMode::Rpc(
        r#"{"jsonrpc":"2.0","id":1,"result":"0001abcd"}"#.into(),
    ));
    let ready = probe_once(
        &client,
        &fixture.url(),
        Probe::ZakuraTip,
        Deadline::after(policy.readiness),
        Cancellation::Ignore,
        &policy,
    )
    .unwrap();
    assert!(ready);
    assert!(fixture.request_body().contains("getbestblockhash"));
    fixture.finish();

    for payload in [
        r#"{"result":{"hash":"0001"}}"#,
        "not-json",
        r#"{"error":"no"}"#,
    ] {
        let mut fixture = HttpFixture::new(HttpMode::Rpc(payload.into()));
        let ready = probe_once(
            &client,
            &fixture.url(),
            Probe::ZakuraTip,
            Deadline::after(policy.readiness),
            Cancellation::Ignore,
            &policy,
        )
        .unwrap();
        assert!(!ready, "{payload}");
        fixture.finish();
    }
}

#[test]
fn failed_or_malformed_inspection_is_not_reported_as_stopped() {
    let policy = short_policy();
    let mut fixture = HttpFixture::new(HttpMode::Health(503));
    let (_sender, shutdown) = Shutdown::channel();
    let docker = ScriptedDocker {
        running: Err("inspect boom".into()),
        logs: Ok("unused".into()),
    };
    let failure = wait_ready(
        &fixture.url(),
        "app-id",
        Deadline::after(policy.readiness),
        &shutdown,
        &policy,
        &docker,
    )
    .unwrap_err();
    let text = format!("{failure:#}");
    assert!(text.contains("Docker inspection failed"), "{text}");
    assert!(text.contains("inspect boom"), "{text}");
    assert!(!text.contains("exited before"), "{text}");
    fixture.finish();

    let mut fixture = HttpFixture::new(HttpMode::Health(503));
    let docker = ScriptedDocker {
        running: Ok("maybe".into()),
        logs: Ok("unused".into()),
    };
    let failure = wait_ready(
        &fixture.url(),
        "app-id",
        Deadline::after(policy.readiness),
        &shutdown,
        &policy,
        &docker,
    )
    .unwrap_err();
    let text = format!("{failure:#}");
    assert!(text.contains("malformed"), "{text}");
    assert!(!text.contains("exited before"), "{text}");
    fixture.finish();
}

#[test]
fn stopped_container_keeps_log_failures_with_the_readiness_error() {
    let policy = short_policy();
    let mut fixture = HttpFixture::new(HttpMode::Rpc(r#"{"result":1}"#.into()));
    let (_sender, shutdown) = Shutdown::channel();
    let docker = ScriptedDocker {
        running: Ok("false".into()),
        logs: Err("logs unavailable".into()),
    };
    let failure = wait_for_zakura_tip(
        &fixture.url(),
        "zakura-id",
        Deadline::after(policy.readiness),
        &shutdown,
        &policy,
        &docker,
    )
    .unwrap_err();
    let text = format!("{failure:#}");
    assert!(text.contains("Zakura exited before"), "{text}");
    assert!(text.contains("diagnostic logs failed"), "{text}");
    assert!(text.contains("logs unavailable"), "{text}");
    fixture.finish();
}

#[test]
fn hanging_diagnostic_logs_expire_on_the_readiness_budget() {
    let policy = short_policy();
    let mut fixture = HttpFixture::new(HttpMode::Health(503));
    let (_sender, shutdown) = Shutdown::channel();
    let context = LifecycleContext::new(policy);
    let script = hanging_log_script();
    let docker = LifecycleDocker {
        context: &context,
        enclosing_deadline: Some(Deadline::after(policy.readiness)),
        command_cap: policy.readiness_docker,
        cancellation: Cancellation::Ignore,
        program: "/bin/sh".into(),
        prefix_args: vec![script.path().join("docker").to_string_lossy().into_owned()],
        env: Vec::new(),
        child_ops: None,
    };
    let started = Instant::now();
    let failure = wait_ready(
        &fixture.url(),
        "app-id",
        Deadline::after(policy.readiness),
        &shutdown,
        &policy,
        &docker,
    )
    .unwrap_err();
    let text = format!("{failure:#}");
    assert!(text.contains("diagnostic logs failed"), "{text}");
    assert!(text.contains("exited before"), "{text}");
    assert!(
        started.elapsed() < policy.readiness + Duration::from_millis(500),
        "logs took {:?}",
        started.elapsed()
    );
    fixture.finish();
}

#[test]
fn run_deadline_interrupts_a_blocked_network_create() {
    let script = tempfile::tempdir().unwrap();
    let marker = script.path().join("network-create-started");
    let path = script.path().join("docker");
    fs::write(
        &path,
        "#!/bin/sh\nif [ \"$1\" = network ] && [ \"$2\" = create ]; then\n  touch \"$THS_MARKER\"\n  sleep 3\nfi\n",
    )
    .unwrap();
    make_executable(&path);
    let mut policy = short_policy();
    policy.startup_docker = Duration::from_secs(5);
    let mut context = LifecycleContext::new(policy);
    context.overall_deadline = Some(Deadline::after(Duration::from_millis(800)));
    let stage = context.startup_deadline(policy.startup_docker);
    let mut docker = script_docker(&script, &context, policy.startup_docker)
        .scoped(stage, policy.startup_docker);
    docker
        .env
        .push(("THS_MARKER".into(), marker.to_string_lossy().into_owned()));
    let root = tempfile::tempdir().unwrap();
    let instance = name("alpha");
    let mut journal =
        RecoveryJournal::create(root.path().join("recovery.json"), &instance).unwrap();
    let started = Instant::now();
    let error =
        ensure_network_with("ths-alpha", &instance, &docker, &mut journal, stage).unwrap_err();
    assert!(
        marker.exists(),
        "the fake Docker command did not reach network create: {error:#}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "network create ignored the run deadline: {:?}",
        started.elapsed()
    );
    assert!(format!("{error:#}").contains("timed out"), "{error:#}");
    assert!(
        context
            .finish_helpers(Deadline::after(policy.cleanup))
            .is_empty()
    );
}
