use super::*;
#[test]
fn run_timeout_after_allocation_skips_child_and_cleans_its_instance() {
    let (mut host, events) = RecordingHost::new();
    host.allocate_delay = Duration::from_millis(80);
    let mut context = LifecycleContext::new(LifecyclePolicy::default());
    context.overall_deadline = Some(Deadline::after(Duration::from_millis(30)));
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();
    let error = runtime
        .run_with_policy(
            &name("alpha"),
            &[OsString::from("true")],
            &runtime.root,
            &host,
            &shutdown,
            &context,
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("timeout"), "{error:#}");
    let events = events.lock().unwrap();
    assert!(events.contains(&"allocate:alpha".to_owned()));
    assert!(events.contains(&"delete:alpha".to_owned()));
    assert!(
        !events
            .iter()
            .any(|event| event == "wait_ready" || event.starts_with("child:"))
    );
}

#[test]
fn settlement_does_not_accept_a_late_status_response() {
    let (_sender, shutdown) = Shutdown::channel();
    let started = Instant::now();
    let error = wait_until_settled(
        &|_| {
            std::thread::sleep(Duration::from_millis(80));
            Ok(RuntimeStatus {
                node_height: Some(1),
                lightwalletd_height: Some(1),
                wallet_scanned_height: Some(1),
                wallet_sync_state: "ready".into(),
                settled: true,
                settlement_blocker: None,
            })
        },
        started + Duration::from_millis(30),
        Duration::from_millis(750),
        &shutdown,
        false,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("timeout"), "{error:#}");
    assert!(started.elapsed() < Duration::from_millis(250));
}

fn settled_status() -> RuntimeStatus {
    RuntimeStatus {
        node_height: Some(101),
        lightwalletd_height: Some(101),
        wallet_scanned_height: Some(101),
        wallet_sync_state: "ready".into(),
        settled: true,
        settlement_blocker: None,
    }
}

/// writes the metadata for `name` after `delay` and marks its session ready, as `start` does;
/// joining hands back `lock` and the readiness lock.
fn finish_startup_after(
    runtime: &Runtime,
    name: &InstanceName,
    lock: InstanceLock,
    delay: Duration,
    dashboard: String,
) -> std::thread::JoinHandle<(InstanceLock, File)> {
    let runtime = Runtime {
        root: runtime.root.clone(),
    };
    let name = name.clone();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        fs::create_dir_all(runtime.instance_dir(&name)).unwrap();
        let endpoints = Endpoints {
            dashboard,
            ..endpoints_for(&host_ports(0).unwrap())
        };
        let ready = mark_ready(&runtime, &name);
        runtime.write_instance(&name, &endpoints).unwrap();
        (lock, ready)
    })
}

fn mark_ready(runtime: &Runtime, name: &InstanceName) -> File {
    let locks = runtime.root.join(".locks");
    let session = locks.join(format!("{name}.session"));
    if !session.exists() {
        fs::write(&session, "live").unwrap();
    }
    let ready = locks.join(format!("{name}.ready"));
    fs::copy(session, &ready).unwrap();
    let ready = File::open(ready).unwrap();
    ready.try_lock().unwrap();
    ready
}

#[test]
fn wait_follows_an_environment_that_is_still_starting() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let lock = runtime.claim_start(&alpha).unwrap();
    let (url, _requests) = serve(vec![(
        "200 OK",
        serde_json::to_string(&settled_status()).unwrap(),
    )]);
    let startup = finish_startup_after(&runtime, &alpha, lock, Duration::from_millis(300), url);
    runtime.wait(&alpha, Duration::from_secs(10), true).unwrap();
    startup.join().unwrap();
}

#[test]
fn wait_outlasts_the_grace_while_startup_holds_the_lock() {
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let lock = runtime.claim_start(&alpha).unwrap();
    let startup = finish_startup_after(
        &runtime,
        &alpha,
        lock,
        Duration::from_millis(900),
        "http://127.0.0.1:1".into(),
    );
    let status = runtime
        .wait_for_settlement(
            &alpha,
            Instant::now() + Duration::from_secs(10),
            Duration::ZERO,
            &shutdown,
            false,
            &|_, _| Ok(settled_status()),
        )
        .unwrap();
    assert!(status.settled);
    startup.join().unwrap();
}

#[test]
fn wait_rejects_an_environment_that_no_command_is_creating() {
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();
    let started = Instant::now();
    let error = runtime
        .wait_for_settlement(
            &name("alpha"),
            started + Duration::from_secs(10),
            Duration::from_millis(100),
            &shutdown,
            false,
            &|_, _| Ok(settled_status()),
        )
        .unwrap_err();
    assert_eq!(format!("{error:#}"), "instance alpha does not exist");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn wait_ends_when_the_environment_is_deleted() {
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let owner = owned_ready_environment(&runtime, &alpha);
    let dir = runtime.instance_dir(&alpha);
    let deletion = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        fs::remove_dir_all(dir).unwrap();
        drop(owner);
    });
    let started = Instant::now();
    let error = runtime
        .wait_for_settlement(
            &alpha,
            started + Duration::from_secs(10),
            Duration::ZERO,
            &shutdown,
            false,
            &|_, _| bail!("connection refused"),
        )
        .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "environment alpha was deleted while waiting"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    deletion.join().unwrap();
}

#[test]
fn run_preserves_child_exit_code_and_cleans_after_settlement() {
    let (mut host, events) = RecordingHost::new();
    host.run_status = Some(RuntimeStatus {
        node_height: Some(105),
        lightwalletd_height: Some(105),
        wallet_scanned_height: Some(105),
        wallet_sync_state: "ready".into(),
        settled: true,
        settlement_blocker: None,
    });
    let mut context = LifecycleContext::new(LifecyclePolicy::default());
    context.overall_deadline = Some(Deadline::after(Duration::from_secs(1)));
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();
    let code = runtime
        .run_with_policy(
            &name("alpha"),
            &[OsString::from("true")],
            &runtime.root,
            &host,
            &shutdown,
            &context,
        )
        .unwrap();
    assert_eq!(code, 42);
    let events = events.lock().unwrap();
    assert!(events.contains(&"runtime_status".to_owned()));
    assert!(events.contains(&"child:alpha".to_owned()));
    assert_eq!(events.last().map(String::as_str), Some("delete:alpha"));
}

#[test]
fn failed_cleanup_reports_error_without_replacing_child_exit_code() {
    let (mut host, _events) = RecordingHost::new();
    host.run_status = Some(RuntimeStatus {
        node_height: Some(105),
        lightwalletd_height: Some(105),
        wallet_scanned_height: Some(105),
        wallet_sync_state: "ready".into(),
        settled: true,
        settlement_blocker: None,
    });
    host.final_cleanup_failure = Some("Docker cleanup failed".into());
    let mut context = LifecycleContext::new(LifecyclePolicy::default());
    context.overall_deadline = Some(Deadline::after(Duration::from_secs(1)));
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();
    let code = runtime
        .run_with_policy(
            &name("alpha"),
            &[OsString::from("true")],
            &runtime.root,
            &host,
            &shutdown,
            &context,
        )
        .unwrap();
    assert_eq!(code, 42);
}

#[test]
fn run_refuses_existing_instance_without_cleanup() {
    let (host, events) = RecordingHost::new();
    let mut context = LifecycleContext::new(LifecyclePolicy::default());
    context.overall_deadline = Some(Deadline::after(Duration::from_secs(1)));
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();
    fs::create_dir_all(runtime.instance_dir(&name("alpha"))).unwrap();
    let error = runtime
        .run_with_policy(
            &name("alpha"),
            &[OsString::from("true")],
            &runtime.root,
            &host,
            &shutdown,
            &context,
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("already exists"));
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn stop_does_not_delete_an_instance_locked_by_another_command() {
    let runtime = runtime_for_tests();
    let lock = runtime.lock_instance(&name("alpha")).unwrap();
    let error = runtime.stop(&name("alpha")).unwrap_err();
    assert!(format!("{error:#}").contains("already in use"));
    drop(lock);
}

#[test]
fn child_receives_project_directory_and_regtest_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("child-environment");
    let command = vec![
        OsString::from("sh"),
        OsString::from("-c"),
        OsString::from("{ pwd; env; } > \"$1\""),
        OsString::from("sh"),
        output.as_os_str().to_owned(),
    ];
    let (_sender, shutdown) = Shutdown::channel();
    let outcome = run_child(
        &command,
        dir.path(),
        &name("alpha"),
        &endpoints_for(&host_ports(0).unwrap()),
        &shutdown,
        Duration::from_millis(50),
    )
    .unwrap();
    assert_eq!(outcome, ChildOutcome::Exited(0));
    let output = fs::read_to_string(output).unwrap();
    assert_eq!(
        output.lines().next(),
        fs::canonicalize(dir.path()).unwrap().to_str()
    );
    for expected in [
        "THS_INSTANCE=alpha",
        "THS_NETWORK=regtest",
        "THS_API_URL=http://127.0.0.1:32805",
        "THS_ZAKURA_RPC_URL=http://127.0.0.1:18232",
        "THS_LIGHTWALLETD_URL=http://127.0.0.1:9067",
        "THS_LIGHTWALLETD_TLS=false",
        "THS_P2P_ADDR=127.0.0.1:18233",
    ] {
        assert!(
            output.lines().any(|line| line == expected),
            "missing {expected}"
        );
    }
    assert!(!output.lines().any(|line| line.starts_with("THS_SEED=")));
}

#[test]
fn child_exit_code_is_preserved() {
    let command = [
        OsString::from("sh"),
        OsString::from("-c"),
        OsString::from("exit 7"),
    ];
    let (_sender, shutdown) = Shutdown::channel();
    let outcome = run_child(
        &command,
        Path::new("."),
        &name("alpha"),
        &endpoints_for(&host_ports(0).unwrap()),
        &shutdown,
        Duration::from_millis(50),
    )
    .unwrap();
    assert_eq!(outcome, ChildOutcome::Exited(7));
}

#[cfg(unix)]
#[test]
fn interrupted_child_terminates_its_background_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("descendant.pid");
    let command = vec![
        OsString::from("sh"),
        OsString::from("-c"),
        OsString::from("sleep 30 & echo $! > \"$1\"; wait"),
        OsString::from("sh"),
        pid_file.as_os_str().to_owned(),
    ];
    let (sender, shutdown) = Shutdown::channel();
    let signal_file = pid_file.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !signal_file.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        sender.send(Ended::Interrupted).unwrap();
    });
    let outcome = run_child(
        &command,
        dir.path(),
        &name("alpha"),
        &endpoints_for(&host_ports(0).unwrap()),
        &shutdown,
        Duration::from_millis(50),
    )
    .unwrap();
    assert_eq!(outcome, ChildOutcome::Signaled);
    let pid = fs::read_to_string(pid_file).unwrap();
    let alive = Command::new("kill")
        .args(["-0", pid.trim()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    assert!(!alive, "background descendant {} survived", pid.trim());
}

#[test]
fn stop_routes_through_a_running_run() {
    let runtime = runtime_for_tests();
    let root = runtime.root.clone();
    let received_signal = root.join("child-signal");
    let script = format!(
        "trap 'echo INT > {0}; exit 0' INT; trap 'echo TERM > {0}; exit 0' TERM; while :; do sleep 0.05; done",
        received_signal.display()
    );
    let (handoff, received) = mpsc::channel();
    let owner = thread::spawn(move || {
        let (mut host, events) = RecordingHost::new();
        host.real_child = true;
        host.run_status = Some(RuntimeStatus {
            node_height: Some(105),
            lightwalletd_height: Some(105),
            wallet_scanned_height: Some(105),
            wallet_sync_state: "ready".into(),
            settled: true,
            settlement_blocker: None,
        });
        handoff.send(events.clone()).unwrap();
        let mut context = LifecycleContext::new(LifecyclePolicy::default());
        context.overall_deadline = Some(Deadline::after(Duration::from_secs(30)));
        let (_sender, shutdown) = Shutdown::channel();
        let runtime = Runtime { root };
        let code = runtime
            .run_with_policy(
                &name("alpha"),
                &[
                    OsString::from("sh"),
                    OsString::from("-c"),
                    OsString::from(script),
                ],
                &runtime.root,
                &host,
                &shutdown,
                &context,
            )
            .map_err(|error| format!("{error:#}"));
        (code, events.lock().unwrap().clone())
    });
    let events = received.recv().unwrap();
    startup::wait_for_event(&events, "child:alpha");
    let started = Instant::now();

    let report = startup::stop_after_owner_with(
        &runtime,
        &events,
        "delete:alpha",
        &RecordingDocker::new("", "", ""),
    )
    .unwrap();

    assert!(report.verified());
    assert!(started.elapsed() < Duration::from_secs(10));
    let (code, events) = owner.join().unwrap();
    assert_eq!(code, Ok(1));
    assert_eq!(events.last().map(String::as_str), Some("delete:alpha"));
    assert!(startup::session_files(&runtime).is_empty());
    assert_eq!(
        fs::read_to_string(&received_signal).unwrap().trim(),
        "INT",
        "stop must reach the child the way Ctrl+C does"
    );
}

#[test]
fn wait_ignores_metadata_left_by_a_previous_owner() {
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let session = runtime.root.join(".locks/alpha.session");
    let lock = runtime.claim_start(&alpha).unwrap();
    fs::write(&session, "old-session").unwrap();
    fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
    runtime
        .write_instance(&alpha, &endpoints_for(&host_ports(0).unwrap()))
        .unwrap();
    fs::write(&session, "new-session").unwrap();
    let delay = Duration::from_millis(300);
    let startup = finish_startup_after(&runtime, &alpha, lock, delay, "http://127.0.0.1:1".into());
    let started = Instant::now();

    let status = runtime
        .wait_for_settlement(
            &alpha,
            Instant::now() + Duration::from_secs(10),
            Duration::ZERO,
            &shutdown,
            false,
            &|_, _| Ok(settled_status()),
        )
        .unwrap();

    assert!(status.settled);
    assert!(started.elapsed() >= delay);
    assert_eq!(
        runtime.read_instance(&alpha).unwrap().session.as_deref(),
        Some("new-session")
    );
    startup.join().unwrap();
}

#[test]
fn wait_does_not_settle_while_a_stop_is_pending() {
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let _lock = runtime.claim_start(&alpha).unwrap();
    fs::write(runtime.root.join(".locks/alpha.session"), "current").unwrap();
    let ready = mark_ready(&runtime, &alpha);
    fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
    runtime
        .write_instance(&alpha, &endpoints_for(&host_ports(0).unwrap()))
        .unwrap();
    let request = runtime.root.join(".locks/alpha.current.stop");
    fs::write(&request, "").unwrap();
    let delay = Duration::from_millis(300);
    let withdraw = {
        let request = request.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            fs::remove_file(request).unwrap();
        })
    };
    let started = Instant::now();

    runtime
        .wait_for_settlement(
            &alpha,
            Instant::now() + Duration::from_secs(10),
            Duration::ZERO,
            &shutdown,
            false,
            &|_, _| Ok(settled_status()),
        )
        .unwrap();

    assert!(started.elapsed() >= delay);
    withdraw.join().unwrap();
    drop(ready);
}

#[test]
fn a_run_marks_its_own_teardown_as_stopping() {
    let (mut host, _events) = RecordingHost::new();
    host.run_status = Some(settled_status());
    let seen = host.stopping_seen.clone();
    let mut context = LifecycleContext::new(LifecyclePolicy::default());
    context.overall_deadline = Some(Deadline::after(Duration::from_secs(30)));
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();

    runtime
        .run_with_policy(
            &name("alpha"),
            &[OsString::from("true")],
            &runtime.root,
            &host,
            &shutdown,
            &context,
        )
        .unwrap();

    assert!(seen.load(Ordering::SeqCst));
    assert!(!runtime.root.join(".locks/alpha.stopping").exists());
}

fn write_instance_without_session(runtime: &Runtime, name: &InstanceName) {
    fs::create_dir_all(runtime.instance_dir(name)).unwrap();
    runtime
        .write_instance(name, &endpoints_for(&host_ports(0).unwrap()))
        .unwrap();
    let path = runtime.instance_dir(name).join("instance.json");
    let mut instance: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    instance.as_object_mut().unwrap().remove("session");
    fs::write(&path, serde_json::to_vec(&instance).unwrap()).unwrap();
}

fn settles_within(
    runtime: &Runtime,
    name: &InstanceName,
    limit: Duration,
) -> Result<RuntimeStatus> {
    let (_sender, shutdown) = Shutdown::channel();
    runtime.wait_for_settlement(
        name,
        Instant::now() + limit,
        Duration::ZERO,
        &shutdown,
        false,
        &|_, _| Ok(settled_status()),
    )
}

#[test]
fn wait_ignores_metadata_without_a_session_while_a_launcher_prepares() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let _owner = runtime.claim_start(&alpha).unwrap();
    let lifecycle = runtime.lock_instance(&alpha).unwrap();
    fs::write(runtime.root.join(".locks/alpha.session"), "live").unwrap();
    write_instance_without_session(&runtime, &alpha);
    let delay = Duration::from_millis(300);
    let startup = finish_startup_after(
        &runtime,
        &alpha,
        lifecycle,
        delay,
        "http://127.0.0.1:1".into(),
    );

    let started = Instant::now();
    assert!(
        settles_within(&runtime, &alpha, Duration::from_secs(10))
            .unwrap()
            .settled
    );
    assert!(started.elapsed() >= delay);
    startup.join().unwrap();
}

#[test]
fn wait_ignores_a_stopping_marker_nobody_holds() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    fs::create_dir_all(runtime.root.join(".locks")).unwrap();
    fs::write(runtime.root.join(".locks/alpha.stopping"), "").unwrap();
    fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
    runtime
        .write_instance(&alpha, &endpoints_for(&host_ports(0).unwrap()))
        .unwrap();

    let error = settles_within(&runtime, &alpha, Duration::from_secs(3)).unwrap_err();

    assert!(
        format!("{error:#}").contains("has no running launcher"),
        "{error:#}"
    );
}

#[test]
fn wait_reports_leftovers_of_a_launcher_that_exited() {
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    drop(owned_ready_environment(&runtime, &alpha));
    let started = Instant::now();

    let error = runtime
        .wait_for_settlement(
            &alpha,
            started + Duration::from_secs(10),
            Duration::from_millis(200),
            &shutdown,
            false,
            &|_, _| Ok(settled_status()),
        )
        .unwrap_err();

    assert_eq!(
        format!("{error:#}"),
        "environment alpha has no running launcher; its containers were left by one that exited. Delete them with `ths --name alpha stop`"
    );
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn wait_does_not_settle_before_the_launcher_is_ready() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let _owner = runtime.claim_start(&alpha).unwrap();
    fs::write(runtime.root.join(".locks/alpha.session"), "live").unwrap();
    fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
    runtime
        .write_instance(&alpha, &endpoints_for(&host_ports(0).unwrap()))
        .unwrap();
    let delay = Duration::from_millis(300);
    let ready = {
        let runtime = Runtime {
            root: runtime.root.clone(),
        };
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            mark_ready(&runtime, &name("alpha"))
        })
    };
    let started = Instant::now();

    assert!(
        settles_within(&runtime, &alpha, Duration::from_secs(10))
            .unwrap()
            .settled
    );
    assert!(started.elapsed() >= delay);
    ready.join().unwrap();
}

fn owned_ready_environment(runtime: &Runtime, name: &InstanceName) -> (InstanceLock, File) {
    let owner = runtime.claim_start(name).unwrap();
    fs::write(
        runtime.root.join(".locks").join(format!("{name}.session")),
        "live",
    )
    .unwrap();
    let ready = mark_ready(runtime, name);
    fs::create_dir_all(runtime.instance_dir(name)).unwrap();
    runtime
        .write_instance(name, &endpoints_for(&host_ports(0).unwrap()))
        .unwrap();
    (owner, ready)
}

fn wait_with_reader(
    runtime: &Runtime,
    name: &InstanceName,
    limit: Duration,
    read_status: &dyn Fn(&Endpoints, Duration) -> Result<RuntimeStatus>,
) -> Result<RuntimeStatus> {
    let (_sender, shutdown) = Shutdown::channel();
    runtime.wait_for_settlement(
        name,
        Instant::now() + limit,
        Duration::ZERO,
        &shutdown,
        false,
        read_status,
    )
}

#[test]
fn wait_rejects_a_settled_answer_when_a_stop_arrives_mid_read() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let _owner = owned_ready_environment(&runtime, &alpha);
    let request = runtime.root.join(".locks/alpha.live.stop");

    let error = wait_with_reader(&runtime, &alpha, Duration::from_millis(1500), &|_, _| {
        fs::write(&request, "").unwrap();
        Ok(settled_status())
    })
    .unwrap_err();

    assert!(format!("{error:#}").contains("being stopped"), "{error:#}");
}

#[test]
fn wait_rejects_a_settled_answer_from_a_replaced_generation() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let _owner = owned_ready_environment(&runtime, &alpha);
    let replacement = Endpoints {
        dashboard: "http://127.0.0.1:2".into(),
        ..endpoints_for(&host_ports(0).unwrap())
    };
    let reads = std::sync::Mutex::new(Vec::new());

    let status = wait_with_reader(
        &runtime,
        &alpha,
        Duration::from_secs(10),
        &|endpoints, _| {
            let mut reads = reads.lock().unwrap();
            if reads.is_empty() {
                fs::write(runtime.root.join(".locks/alpha.session"), "next").unwrap();
                fs::write(runtime.root.join(".locks/alpha.ready"), "next").unwrap();
                runtime.write_instance(&alpha, &replacement).unwrap();
            }
            reads.push(endpoints.dashboard.clone());
            Ok(settled_status())
        },
    )
    .unwrap();

    assert!(status.settled);
    let reads = reads.into_inner().unwrap();
    assert!(reads.len() >= 2, "{reads:?}");
    assert_eq!(reads.last().unwrap(), "http://127.0.0.1:2");
}

#[test]
fn wait_rejects_a_settled_answer_after_the_metadata_is_deleted() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let owner = owned_ready_environment(&runtime, &alpha);
    let owner = std::sync::Mutex::new(Some(owner));

    let error = wait_with_reader(&runtime, &alpha, Duration::from_secs(10), &|_, _| {
        fs::remove_file(runtime.instance_dir(&alpha).join("instance.json")).unwrap();
        fs::remove_file(runtime.root.join(".locks/alpha.session")).unwrap();
        drop(owner.lock().unwrap().take());
        Ok(settled_status())
    })
    .unwrap_err();

    assert!(
        format!("{error:#}").contains("deleted while waiting"),
        "{error:#}"
    );
}

#[test]
fn wait_rejects_a_settled_answer_read_across_the_owners_exit() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let owner = owned_ready_environment(&runtime, &alpha);
    let owner = std::sync::Mutex::new(Some(owner));
    let reads = std::sync::atomic::AtomicUsize::new(0);

    let result = wait_with_reader(&runtime, &alpha, Duration::from_millis(1500), &|_, _| {
        if reads.fetch_add(1, Ordering::SeqCst) == 0 {
            fs::remove_file(runtime.root.join(".locks/alpha.session")).unwrap();
            drop(owner.lock().unwrap().take());
            Ok(settled_status())
        } else {
            Err(anyhow!("connection refused"))
        }
    });

    assert!(result.is_err(), "{result:?}");
}

#[test]
fn wait_rejects_a_settled_answer_read_across_a_stop_without_an_owner() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
    runtime
        .write_instance(&alpha, &endpoints_for(&host_ports(0).unwrap()))
        .unwrap();
    let reads = std::sync::atomic::AtomicUsize::new(0);

    let result = wait_with_reader(&runtime, &alpha, Duration::from_millis(1500), &|_, _| {
        if reads.fetch_add(1, Ordering::SeqCst) == 0 {
            drop(runtime.claim_start(&alpha).unwrap());
            Ok(settled_status())
        } else {
            Err(anyhow!("connection refused"))
        }
    });

    assert!(result.is_err(), "{result:?}");
}

#[test]
fn wait_ignores_the_readiness_of_a_launcher_that_died() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    drop(owned_ready_environment(&runtime, &alpha));
    let next = runtime.claim_start(&alpha).unwrap();

    let error = settles_within(&runtime, &alpha, Duration::from_millis(500)).unwrap_err();

    assert!(format!("{error:#}").contains("still starting"), "{error:#}");
    drop(next);
}
