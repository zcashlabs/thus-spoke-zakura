use super::*;

#[test]
fn dropping_shutdown_joins_signal_worker_without_a_signal() {
    let shutdown = Shutdown::install().unwrap();
    let worker_flag = Arc::downgrade(&shutdown.flag);

    drop(shutdown);

    let deadline = Instant::now() + Duration::from_secs(1);
    while worker_flag.upgrade().is_some() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        worker_flag.upgrade().is_none(),
        "signal worker survived drop"
    );
}

#[test]
fn partial_startup_cleanup_removes_only_owned_resources() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let metadata = runtime.instance_dir(&name("alpha"));
    fs::create_dir_all(&metadata).unwrap();
    fs::write(metadata.join("instance.json"), "partial startup").unwrap();
    let mut docker = RecordingDocker::new("", "ths-alpha-chain\nths-alpha-wallet", "ths-alpha");
    docker.inspect(
        "volume",
        "ths-alpha-chain",
        r#"[{"Name":"ths-alpha-chain","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
    );
    docker.inspect(
        "volume",
        "ths-alpha-wallet",
        r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"other"}}]"#,
    );
    docker.inspect(
        "network",
        "ths-alpha",
        r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
    );

    let error = runtime
        .delete_instance_resources_with_mode(&name("alpha"), &docker, true)
        .unwrap_err();

    assert!(format!("{error:#}").contains("not owned"), "{error:#}");
    assert_eq!(
        *docker.runs.lock().unwrap(),
        ["volume rm ths-alpha-chain", "network rm owned-network"]
    );
    assert_eq!(
        fs::read_to_string(metadata.join("instance.json")).unwrap(),
        "partial startup"
    );
}

#[test]
fn shutdown_collision_removes_owned_resources_and_preserves_foreign_container() {
    let mut docker = RecordingDocker::new(
        "ths-alpha-app\nths-alpha-init",
        "ths-alpha-wallet",
        "ths-alpha",
    );
    docker.inspect(
        "container",
        "ths-alpha-app",
        r#"[{"Id":"owned-app","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
    );
    docker.inspect(
        "container",
        "ths-alpha-init",
        r#"[{"Id":"foreign-init","Config":{"Labels":{}}}]"#,
    );
    docker.inspect(
        "volume",
        "ths-alpha-wallet",
        r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
    );
    docker.inspect(
        "network",
        "ths-alpha",
        r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
    );
    let (mut host, events) = RecordingHost::new();
    host.shutdown_collision = Some(docker);
    let (_sender, shutdown) = Shutdown::channel();

    let error = runtime_for_tests()
        .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
        .unwrap_err();

    assert!(format!("{error:#}").contains("ths-alpha-init is not owned"));
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"delete_partial:alpha".into())
    );
    assert_eq!(
        *host
            .shutdown_collision
            .as_ref()
            .unwrap()
            .runs
            .lock()
            .unwrap(),
        [
            "rm -f owned-app",
            "volume rm ths-alpha-wallet",
            "network rm owned-network",
        ]
    );
}

#[test]
fn browser_open_failure_keeps_environment_running_until_shutdown() {
    let (mut host, events) = RecordingHost::new();
    host.open_url_result = Err("opening http://127.0.0.1:1".into());
    let (sender, shutdown) = Shutdown::channel();
    let watched = Arc::clone(&events);
    thread::spawn(move || {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            let waiting = watched
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.starts_with("wait_for_shutdown"));
            if waiting {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        sender.send(Ended::Interrupted).unwrap();
    });

    runtime_for_tests()
        .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
        .unwrap();

    let events = events.lock().unwrap().clone();
    let open_url = events
        .iter()
        .position(|event| event.starts_with("open_url:"))
        .unwrap();
    let wait_for_shutdown = events
        .iter()
        .position(|event| event.starts_with("wait_for_shutdown:"))
        .unwrap();
    let final_delete = events
        .iter()
        .rposition(|event| event == "delete:alpha")
        .unwrap();
    assert!(open_url < wait_for_shutdown);
    assert!(wait_for_shutdown < final_delete);
}

#[test]
fn no_open_skips_browser_and_waits_for_shutdown() {
    let (host, events) = RecordingHost::new();
    let (sender, shutdown) = Shutdown::channel();
    let watched = std::sync::Arc::clone(&events);
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            let waiting = watched
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.starts_with("wait_for_shutdown"));
            if waiting {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        sender.send(Ended::Interrupted).unwrap();
    });
    runtime_for_tests()
        .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
        .unwrap();
    let events = events.lock().unwrap().clone();
    assert!(!events.iter().any(|e| e.starts_with("open_url:")));
    assert!(events.iter().any(|e| e.starts_with("wait_for_shutdown")));
    assert!(events.iter().any(|e| e == "delete:alpha"));
}

#[test]
fn startup_and_cleanup_failures_are_both_reported_once() {
    let (mut host, events) = RecordingHost::new();
    host.wait_ready_result = Err("dashboard readiness timed out".into());
    host.final_cleanup_failure = Some("Docker inspection failed".into());
    let (_sender, shutdown) = Shutdown::channel();
    let error = runtime_for_tests()
        .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("dashboard readiness timed out"));
    assert!(text.contains("Docker inspection failed"));
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| *event == "final_cleanup:alpha")
            .count(),
        1
    );
}

#[test]
fn interrupt_before_ready_deletes_the_started_instance() {
    let (mut host, events) = RecordingHost::new();
    host.interrupt_before_ready = true;
    let (_sender, shutdown) = Shutdown::channel();
    let err = runtime_for_tests()
        .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
        .unwrap_err();
    assert!(err.to_string().contains("interrupted"));
    let events = events.lock().unwrap().clone();
    assert!(events.iter().any(|e| e == "allocate:alpha"));
    assert!(events.iter().any(|e| e == "delete:alpha"));
    assert!(!events.iter().any(|e| e.starts_with("wait_for_shutdown")));
}

#[test]
fn interrupt_during_allocate_deletes_only_the_named_instance() {
    let (host, events) = RecordingHost::new();
    let (sender, shutdown) = Shutdown::channel();
    sender.send(Ended::Interrupted).unwrap();
    let err = runtime_for_tests()
        .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
        .unwrap_err();
    assert!(err.to_string().contains("interrupted"));
    let events = events.lock().unwrap().clone();
    assert!(events.iter().any(|e| e == "delete:alpha"));
    assert!(!events.iter().any(|e| e.contains("beta")));
}

#[test]
fn shutdown_check_bails_when_latched() {
    let (sender, shutdown) = Shutdown::channel();
    shutdown.check().unwrap();
    sender.send(Ended::Interrupted).unwrap();
    let err = shutdown.check().unwrap_err();
    assert!(err.to_string().contains("interrupted"));
    let err = shutdown.check().unwrap_err();
    assert!(err.to_string().contains("interrupted"));
}

#[test]
fn shutdown_wait_timeout_wakes_on_signal() {
    let (sender, shutdown) = Shutdown::channel();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        sender.send(Ended::Interrupted).unwrap();
    });
    let started = Instant::now();
    let err = shutdown.wait_timeout(Duration::from_secs(2)).unwrap_err();
    assert!(err.to_string().contains("interrupted"));
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn shutdown_wait_timeout_returns_on_idle() {
    let (_sender, shutdown) = Shutdown::channel();
    shutdown.wait_timeout(Duration::from_millis(20)).unwrap();
}

#[test]
fn shutdown_wait_returns_after_signal() {
    let (sender, shutdown) = Shutdown::channel();
    sender.send(Ended::Interrupted).unwrap();
    assert_eq!(shutdown.wait().unwrap(), Ended::Interrupted);
}

#[test]
fn wait_ready_aborts_when_shutdown_is_signaled() {
    let (sender, shutdown) = Shutdown::channel();
    sender.send(Ended::Interrupted).unwrap();
    let err = readiness::wait_ready(
        "http://127.0.0.1:1",
        "missing-app",
        Deadline::after(Duration::from_secs(5)),
        &shutdown,
        &LifecyclePolicy::default(),
        &IdleDocker,
    )
    .unwrap_err();
    assert!(err.to_string().contains("interrupted"));
}

#[test]
fn wait_for_zakura_tip_aborts_when_shutdown_is_signaled() {
    let (sender, shutdown) = Shutdown::channel();
    sender.send(Ended::Interrupted).unwrap();
    let err = readiness::wait_for_zakura_tip(
        "http://127.0.0.1:1",
        "missing-zakura",
        Deadline::after(Duration::from_secs(5)),
        &shutdown,
        &LifecyclePolicy::default(),
        &IdleDocker,
    )
    .unwrap_err();
    assert!(err.to_string().contains("interrupted"));
}

#[test]
fn deletion_elsewhere_ends_start_without_deleting_again() {
    let (mut host, events) = RecordingHost::new();
    host.deleted_elsewhere = true;
    let (_sender, shutdown) = Shutdown::channel();
    runtime_for_tests()
        .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
        .unwrap();
    let events = events.lock().unwrap().clone();
    let waited = events
        .iter()
        .position(|e| e == "wait_for_shutdown:app-1")
        .expect("waited on the app container");
    assert!(
        !events[waited..].iter().any(|e| e.starts_with("delete:")),
        "deleted again after another command deleted it: {events:?}"
    );
}
#[test]
fn shutdown_wait_returns_when_the_environment_is_gone() {
    let (sender, shutdown) = Shutdown::channel();
    sender.send(Ended::DeletedElsewhere).unwrap();
    assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
    assert!(!shutdown.try_interrupted());
}
#[test]
fn shutdown_wait_keeps_a_deletion_notice_that_arrived_first() {
    let (sender, shutdown) = Shutdown::channel();
    sender.send(Ended::DeletedElsewhere).unwrap();
    sender.send(Ended::Interrupted).unwrap();
    assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
    assert!(shutdown.try_interrupted());
}

fn metadata_snapshot(dir: &Path) -> Vec<(OsString, Vec<u8>)> {
    let mut entries = fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

#[test]
fn second_start_is_refused_before_touching_a_running_environment() {
    let runtime = runtime_for_tests();
    let root = runtime.root.clone();
    let observed = Arc::new(Mutex::new(None));
    let (mut first, first_events) = RecordingHost::new();
    let record = observed.clone();
    first.while_running = Some(Box::new(move || {
        let second = Runtime { root: root.clone() };
        let metadata = second.instance_dir(&name("alpha"));
        fs::write(metadata.join("instance.json"), "first launcher").unwrap();
        let before = metadata_snapshot(&metadata);
        let (mut host, events) = RecordingHost::new();
        host.while_running = Some(Box::new(|| Ended::Interrupted));
        let (_sender, shutdown) = Shutdown::channel();
        let result = second
            .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
            .map_err(|error| format!("{error:#}"));
        let unchanged = metadata_snapshot(&metadata) == before;
        *record.lock().unwrap() = Some((result, events.lock().unwrap().clone(), unchanged));
        Ended::Interrupted
    }));
    let (_sender, shutdown) = Shutdown::channel();

    runtime
        .start_with(&name("alpha"), true, false, 0, &first, &shutdown)
        .unwrap();

    let (result, second_events, unchanged) = observed.lock().unwrap().take().unwrap();
    let error = result.expect_err("a second start replaced the running environment");
    assert!(
        error.contains("already running in another `ths start`"),
        "{error}"
    );
    assert!(second_events.is_empty(), "{second_events:?}");
    assert!(unchanged, "the second start changed instance metadata");
    assert!(
        first_events
            .lock()
            .unwrap()
            .contains(&"final_cleanup:alpha".into())
    );
}

#[test]
fn stopped_launcher_keeps_the_name_until_its_cleanup_ends() {
    let runtime = runtime_for_tests();
    let root = runtime.root.clone();
    let observed = Arc::new(Mutex::new(None));
    let (mut first, first_events) = RecordingHost::new();
    let record = observed.clone();
    first.while_running = Some(Box::new(move || {
        let other_shell = Runtime { root: root.clone() };
        let stop = other_shell.lock_instance(&name("alpha")).map(drop);
        let (mut host, events) = RecordingHost::new();
        host.while_running = Some(Box::new(|| Ended::DeletedElsewhere));
        let (_sender, shutdown) = Shutdown::channel();
        let restart = other_shell
            .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
            .map_err(|error| format!("{error:#}"));
        *record.lock().unwrap() = Some((stop.is_ok(), restart, events.lock().unwrap().clone()));
        Ended::Interrupted
    }));
    let (_sender, shutdown) = Shutdown::channel();

    runtime
        .start_with(&name("alpha"), true, false, 0, &first, &shutdown)
        .unwrap();

    let (stop_admitted, restart, restart_events) = observed.lock().unwrap().take().unwrap();
    assert!(stop_admitted, "stop was locked out by the running launcher");
    let error = restart.expect_err("a new start ran before the old launcher's cleanup");
    assert!(
        error.contains("already running in another `ths start`"),
        "{error}"
    );
    assert!(restart_events.is_empty(), "{restart_events:?}");
    assert!(
        first_events
            .lock()
            .unwrap()
            .contains(&"final_cleanup:alpha".into())
    );

    let (mut next, next_events) = RecordingHost::new();
    next.while_running = Some(Box::new(|| Ended::DeletedElsewhere));
    let (_sender, shutdown) = Shutdown::channel();
    runtime
        .start_with(&name("alpha"), true, false, 0, &next, &shutdown)
        .unwrap();
    assert!(
        next_events
            .lock()
            .unwrap()
            .contains(&"allocate:alpha".into())
    );
}

type OwnerOutcome = (Result<(), String>, Vec<String>);

fn spawn_owner(
    root: PathBuf,
    configure: impl FnOnce(&mut RecordingHost) + Send + 'static,
) -> (
    thread::JoinHandle<OwnerOutcome>,
    mpsc::Sender<Ended>,
    Arc<Mutex<Vec<String>>>,
) {
    let (handoff, received) = mpsc::channel();
    let owner = thread::spawn(move || {
        let (mut host, events) = RecordingHost::new();
        configure(&mut host);
        let (interrupt, shutdown) = Shutdown::channel();
        handoff.send((interrupt, events.clone())).unwrap();
        let result = Runtime { root }
            .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
            .map_err(|error| format!("{error:#}"));
        (result, events.lock().unwrap().clone())
    });
    let (interrupt, events) = received.recv().unwrap();
    (owner, interrupt, events)
}

pub(super) fn wait_for_event(events: &Mutex<Vec<String>>, event: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !events.lock().unwrap().iter().any(|e| e == event) {
        assert!(Instant::now() < deadline, "{event} never happened");
        thread::sleep(Duration::from_millis(1));
    }
}

pub(super) fn stop_policy() -> LifecyclePolicy {
    LifecyclePolicy {
        poll: Duration::from_millis(1),
        cleanup: Duration::from_secs(5),
        ..LifecyclePolicy::default()
    }
}

pub(super) fn session_files(runtime: &Runtime) -> Vec<String> {
    fs::read_dir(runtime.root.join(".locks"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|file| {
            !file.ends_with(".lock") && !file.ends_with(".owner") && !file.ends_with(".generation")
        })
        .collect()
}

fn stop_after_owner(runtime: &Runtime, owner: &Mutex<Vec<String>>) -> Result<CleanupReport> {
    stop_after_owner_with(
        runtime,
        owner,
        "final_cleanup:alpha",
        &RecordingDocker::new("", "", ""),
    )
}

pub(super) fn stop_after_owner_with(
    runtime: &Runtime,
    owner: &Mutex<Vec<String>>,
    owner_cleanup: &str,
    docker: &RecordingDocker,
) -> Result<CleanupReport> {
    runtime.verified_cleanup_with(&name("alpha"), &stop_policy(), || {
        assert!(
            owner.lock().unwrap().iter().any(|e| e == owner_cleanup),
            "stop cleaned up before the owner did"
        );
        runtime.cleanup_with(&name("alpha"), docker, false, &stop_policy())
    })
}

#[test]
fn stop_during_startup_aborts_the_owner_and_verifies_cleanup() {
    let runtime = runtime_for_tests();
    let (owner, _interrupt, events) = spawn_owner(runtime.root.clone(), |host| {
        host.allocate_delay = Duration::from_millis(300)
    });
    wait_for_event(&events, "allocate:alpha");

    let report = stop_after_owner(&runtime, &events).unwrap();

    assert!(report.verified());
    let (result, events) = owner.join().unwrap();
    assert_eq!(
        result.unwrap_err(),
        "alpha was stopped by another command during startup"
    );
    assert!(!events.iter().any(|e| e == "wait_ready"), "{events:?}");
    assert!(session_files(&runtime).is_empty());
}

#[test]
fn stop_while_ready_routes_through_the_owner() {
    let runtime = runtime_for_tests();
    let (owner, _interrupt, events) = spawn_owner(runtime.root.clone(), |_| {});
    wait_for_event(&events, "wait_for_shutdown:app-1");

    let report = stop_after_owner(&runtime, &events).unwrap();

    assert!(report.verified());
    let (result, events) = owner.join().unwrap();
    result.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| *e == "final_cleanup:alpha")
            .count(),
        1
    );
    assert!(session_files(&runtime).is_empty());
}

#[test]
fn concurrent_stops_both_wait_for_the_owner() {
    let runtime = runtime_for_tests();
    let (owner, _interrupt, events) = spawn_owner(runtime.root.clone(), |_| {});
    wait_for_event(&events, "wait_for_shutdown:app-1");

    let stops = [(); 2].map(|()| {
        let other_shell = Runtime {
            root: runtime.root.clone(),
        };
        let events = events.clone();
        thread::spawn(move || {
            stop_after_owner(&other_shell, &events)
                .map(|report| report.verified())
                .map_err(|error| format!("{error:#}"))
        })
    });

    for stop in stops {
        assert_eq!(stop.join().unwrap(), Ok(true));
    }
    owner.join().unwrap().0.unwrap();
}

#[test]
fn stale_stop_request_does_not_stop_a_new_session() {
    let runtime = runtime_for_tests();
    let locks = runtime.root.join(".locks");
    fs::create_dir_all(&locks).unwrap();
    let stale = locks.join(format!("alpha.{}.stop", uuid::Uuid::new_v4()));
    fs::write(&stale, "").unwrap();
    let (owner, interrupt, events) = spawn_owner(runtime.root.clone(), |_| {});
    wait_for_event(&events, "wait_for_shutdown:app-1");

    thread::sleep(Duration::from_millis(200));

    assert!(
        !owner.is_finished(),
        "a stale request stopped the new session"
    );
    assert!(!stale.exists());
    interrupt.send(Ended::Interrupted).unwrap();
    owner.join().unwrap().0.unwrap();
}

#[test]
fn stop_without_an_owner_cleans_leftovers() {
    let runtime = runtime_for_tests();
    let locks = runtime.root.join(".locks");
    fs::create_dir_all(&locks).unwrap();
    fs::write(locks.join("alpha.session"), "crashed-launcher").unwrap();
    let mut docker = RecordingDocker::new("ths-alpha-app", "ths-alpha-wallet", "ths-alpha");
    docker.inspect(
        "container",
        "ths-alpha-app",
        r#"[{"Id":"owned-app","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
    );
    docker.inspect(
        "volume",
        "ths-alpha-wallet",
        r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
    );
    docker.inspect(
        "network",
        "ths-alpha",
        r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
    );

    let report = runtime
        .verified_cleanup_with(&name("alpha"), &stop_policy(), || {
            runtime.cleanup_with(&name("alpha"), &docker, false, &stop_policy())
        })
        .unwrap();

    assert!(report.verified());
    assert_eq!(
        *docker.runs.lock().unwrap(),
        [
            "rm -f owned-app",
            "volume rm ths-alpha-wallet",
            "network rm owned-network",
        ]
    );
    assert!(session_files(&runtime).is_empty());
}

#[test]
fn a_stop_after_wait_settles_ends_start_cleanly() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let (owner, _interrupt, events) = spawn_owner(runtime.root.clone(), |host| {
        host.ready_delay = Duration::from_millis(300)
    });
    wait_for_event(&events, "wait_ready");
    runtime
        .write_instance(&alpha, &endpoints_for(&host_ports(0).unwrap()))
        .unwrap();
    let (_sender, shutdown) = Shutdown::channel();

    runtime
        .wait_for_settlement(
            &alpha,
            Instant::now() + Duration::from_secs(10),
            Duration::ZERO,
            &shutdown,
            false,
            &|_, _| {
                Ok(RuntimeStatus {
                    node_height: Some(101),
                    lightwalletd_height: Some(101),
                    wallet_scanned_height: Some(101),
                    wallet_sync_state: "ready".into(),
                    settled: true,
                    settlement_blocker: None,
                })
            },
        )
        .unwrap();

    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"wait_for_shutdown:app-1".to_owned())
    );
    assert!(stop_after_owner(&runtime, &events).unwrap().verified());
    owner.join().unwrap().0.unwrap();
}

#[test]
fn a_launcher_that_cannot_record_readiness_says_so() {
    let runtime = runtime_for_tests();
    let (owner, _interrupt, events) = spawn_owner(runtime.root.clone(), |host| {
        host.ready_delay = Duration::from_millis(200)
    });
    wait_for_event(&events, "wait_ready");
    fs::create_dir_all(runtime.root.join(".locks/alpha.ready")).unwrap();

    let (result, events) = owner.join().unwrap();

    let error = result.unwrap_err();
    assert!(error.contains("recording that alpha is ready"), "{error}");
    assert!(!events.contains(&"wait_for_shutdown:app-1".to_owned()));
}

#[test]
fn an_owner_tearing_down_never_looks_ready() {
    use std::os::unix::fs::PermissionsExt;
    let runtime = runtime_for_tests();
    let locks = runtime.root.join(".locks");
    let (mut host, _events) = RecordingHost::new();
    let in_teardown = host.ready_in_teardown.clone();
    let alpha = name("alpha");
    let (root, owned) = (runtime.root.clone(), alpha.clone());
    host.while_running = Some(Box::new(move || {
        let runtime = Runtime { root: root.clone() };
        fs::create_dir_all(runtime.instance_dir(&owned)).unwrap();
        runtime
            .write_instance(&owned, &endpoints_for(&host_ports(0).unwrap()))
            .unwrap();
        fs::set_permissions(root.join(".locks"), fs::Permissions::from_mode(0o555)).unwrap();
        Ended::Interrupted
    }));
    let (_sender, shutdown) = Shutdown::channel();

    let started = runtime.start_with(&alpha, true, false, 0, &host, &shutdown);
    fs::set_permissions(&locks, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(started.is_ok(), "{started:?}");
    assert!(!in_teardown.load(Ordering::SeqCst));
}

#[test]
fn an_owner_that_panics_never_looks_ready_while_it_tears_down() {
    let runtime = runtime_for_tests();
    let (mut host, _events) = RecordingHost::new();
    let in_teardown = host.ready_in_teardown.clone();
    let alpha = name("alpha");
    let (root, owned) = (runtime.root.clone(), alpha.clone());
    host.while_running = Some(Box::new(move || {
        let runtime = Runtime { root: root.clone() };
        fs::create_dir_all(runtime.instance_dir(&owned)).unwrap();
        runtime
            .write_instance(&owned, &endpoints_for(&host_ports(0).unwrap()))
            .unwrap();
        panic!("the owner failed while running")
    }));
    let (_sender, shutdown) = Shutdown::channel();

    let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.start_with(&alpha, true, false, 0, &host, &shutdown)
    }));

    assert!(started.is_err());
    assert!(!in_teardown.load(Ordering::SeqCst));
}

#[test]
fn reset_without_force_does_not_request_shutdown() {
    let runtime = runtime_for_tests();
    let (owner, interrupt, events) = spawn_owner(runtime.root.clone(), |_| {});
    wait_for_event(&events, "wait_for_shutdown:app-1");

    let error = runtime.reset(&name("alpha"), false).unwrap_err();

    assert!(error.to_string().contains("--force"), "{error:#}");
    let mut files = session_files(&runtime);
    files.sort();
    assert_eq!(files, ["alpha.ready", "alpha.session"]);
    assert!(!owner.is_finished());
    interrupt.send(Ended::Interrupted).unwrap();
    owner.join().unwrap().0.unwrap();
}

#[test]
fn stop_refuses_a_launcher_that_replaced_its_target() {
    let runtime = runtime_for_tests();
    let _owner = runtime.claim_start(&name("alpha")).unwrap();
    let locks = runtime.root.join(".locks");
    fs::write(locks.join("alpha.session"), "old-session").unwrap();
    let cleaned = Arc::new(AtomicBool::new(false));
    let other_shell = Runtime {
        root: runtime.root.clone(),
    };
    let ran = cleaned.clone();
    let stop = thread::spawn(move || {
        other_shell
            .verified_cleanup_with(&name("alpha"), &stop_policy(), || {
                ran.store(true, Ordering::SeqCst);
                Ok(verified_report())
            })
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !locks.join("alpha.old-session.stop").exists() {
        assert!(Instant::now() < deadline, "stop never requested shutdown");
        thread::sleep(Duration::from_millis(1));
    }

    fs::write(locks.join("alpha.session"), "new-session").unwrap();

    let error = stop.join().unwrap().unwrap_err();
    assert!(error.contains("a newer one took over"), "{error}");
    assert!(
        error.contains("Run `ths --name alpha stop` again"),
        "{error}"
    );
    assert!(!cleaned.load(Ordering::SeqCst));
}

#[test]
fn a_delayed_stop_keeps_a_replacement_that_exited() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let locks = runtime.root.join(".locks");
    let first = runtime.claim_start(&alpha).unwrap();
    fs::write(locks.join("alpha.session"), "first-session").unwrap();
    let cleaned = Arc::new(AtomicBool::new(false));
    let other_shell = Runtime {
        root: runtime.root.clone(),
    };
    let ran = cleaned.clone();
    let stop = thread::spawn(move || {
        let policy = LifecyclePolicy {
            poll: Duration::from_millis(400),
            ..stop_policy()
        };
        other_shell
            .verified_cleanup_with(&name("alpha"), &policy, || {
                ran.store(true, Ordering::SeqCst);
                Ok(verified_report())
            })
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !locks.join("alpha.first-session.stop").exists() {
        assert!(Instant::now() < deadline, "stop never requested shutdown");
        thread::sleep(Duration::from_millis(1));
    }

    fs::remove_file(locks.join("alpha.session")).unwrap();
    drop(first);
    let replacement = runtime.claim_start(&alpha).unwrap();
    fs::write(locks.join("alpha.session"), "second-session").unwrap();
    fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
    runtime
        .write_instance(&alpha, &endpoints_for(&host_ports(0).unwrap()))
        .unwrap();
    drop(replacement);

    let error = stop.join().unwrap().unwrap_err();
    assert!(error.contains("a newer one took over"), "{error}");
    assert!(!cleaned.load(Ordering::SeqCst));
    assert_eq!(
        fs::read_to_string(locks.join("alpha.session")).unwrap(),
        "second-session"
    );
    assert_eq!(
        runtime.read_instance(&alpha).unwrap().session.as_deref(),
        Some("second-session")
    );
}

#[test]
fn a_delayed_stop_keeps_a_replacement_that_left_only_metadata() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let locks = runtime.root.join(".locks");
    let first = runtime.claim_start(&alpha).unwrap();
    fs::write(locks.join("alpha.session"), "first-session").unwrap();
    let cleaned = Arc::new(AtomicBool::new(false));
    let other_shell = Runtime {
        root: runtime.root.clone(),
    };
    let ran = cleaned.clone();
    let stop = thread::spawn(move || {
        let policy = LifecyclePolicy {
            poll: Duration::from_millis(400),
            ..stop_policy()
        };
        other_shell
            .verified_cleanup_with(&name("alpha"), &policy, || {
                ran.store(true, Ordering::SeqCst);
                Ok(verified_report())
            })
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !locks.join("alpha.first-session.stop").exists() {
        assert!(Instant::now() < deadline, "stop never requested shutdown");
        thread::sleep(Duration::from_millis(1));
    }

    fs::remove_file(locks.join("alpha.session")).unwrap();
    drop(first);
    let replacement = runtime.claim_start(&alpha).unwrap();
    fs::write(locks.join("alpha.session"), "second-session").unwrap();
    fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
    runtime
        .write_instance(&alpha, &endpoints_for(&host_ports(0).unwrap()))
        .unwrap();
    fs::remove_file(locks.join("alpha.session")).unwrap();
    drop(replacement);

    let error = stop.join().unwrap().unwrap_err();
    assert!(error.contains("a newer one took over"), "{error}");
    assert!(!cleaned.load(Ordering::SeqCst));
    assert!(runtime.instance_dir(&alpha).join("instance.json").exists());
}

#[test]
fn a_delayed_stop_keeps_a_replacement_that_failed_before_writing_metadata() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let locks = runtime.root.join(".locks");
    let first = runtime.claim_start(&alpha).unwrap();
    fs::write(locks.join("alpha.session"), "first-session").unwrap();
    let cleaned = Arc::new(AtomicBool::new(false));
    let other_shell = Runtime {
        root: runtime.root.clone(),
    };
    let ran = cleaned.clone();
    let stop = thread::spawn(move || {
        let policy = LifecyclePolicy {
            poll: Duration::from_millis(400),
            ..stop_policy()
        };
        other_shell
            .verified_cleanup_with(&name("alpha"), &policy, || {
                ran.store(true, Ordering::SeqCst);
                Ok(verified_report())
            })
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !locks.join("alpha.first-session.stop").exists() {
        assert!(Instant::now() < deadline, "stop never requested shutdown");
        thread::sleep(Duration::from_millis(1));
    }

    fs::remove_file(locks.join("alpha.session")).unwrap();
    drop(first);
    let (mut replacement, events) = RecordingHost::new();
    replacement.wait_ready_result = Err("container lookup failed".into());
    replacement.final_cleanup_failure = Some("removal failed".into());
    let (_sender, shutdown) = Shutdown::channel();
    let failed = runtime.start_with(&alpha, true, false, 0, &replacement, &shutdown);

    assert!(failed.is_err());
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"allocate:alpha".to_owned())
    );
    assert!(!locks.join("alpha.session").exists());
    assert!(!runtime.instance_dir(&alpha).join("instance.json").exists());
    let error = stop.join().unwrap().unwrap_err();
    assert!(error.contains("a newer one took over"), "{error}");
    assert!(!cleaned.load(Ordering::SeqCst));
}

#[test]
fn a_stop_queued_behind_another_stop_keeps_a_launcher_that_started_after_it() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let locks = runtime.root.join(".locks");
    let (inside, entered) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let first_shell = Runtime {
        root: runtime.root.clone(),
    };
    let first_stop = thread::spawn(move || {
        first_shell
            .verified_cleanup_with(&name("alpha"), &stop_policy(), || {
                inside.send(()).unwrap();
                released.recv().unwrap();
                Ok(verified_report())
            })
            .map(|report| report.verified())
            .map_err(|error| format!("{error:#}"))
    });
    entered.recv().unwrap();
    let cleaned = Arc::new(AtomicBool::new(false));
    let second_shell = Runtime {
        root: runtime.root.clone(),
    };
    let ran = cleaned.clone();
    let second_stop = thread::spawn(move || {
        let policy = LifecyclePolicy {
            poll: Duration::from_millis(400),
            ..stop_policy()
        };
        second_shell
            .verified_cleanup_with(&name("alpha"), &policy, || {
                ran.store(true, Ordering::SeqCst);
                Ok(verified_report())
            })
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    });
    thread::sleep(Duration::from_millis(100));
    release.send(()).unwrap();
    assert_eq!(first_stop.join().unwrap(), Ok(true));

    let replacement = runtime.claim_start(&alpha).unwrap();
    fs::write(locks.join("alpha.session"), "second-session").unwrap();

    let error = second_stop.join().unwrap().unwrap_err();
    assert!(error.contains("another launcher started alpha"), "{error}");
    assert!(!cleaned.load(Ordering::SeqCst));
    assert!(!locks.join("alpha.second-session.stop").exists());
    drop(replacement);
}

#[test]
fn a_stop_keeps_a_replacement_for_a_launcher_that_crashed_before_publishing() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let first = runtime.claim_start(&alpha).unwrap();
    let cleaned = Arc::new(AtomicBool::new(false));
    let other_shell = Runtime {
        root: runtime.root.clone(),
    };
    let ran = cleaned.clone();
    let stop = thread::spawn(move || {
        let policy = LifecyclePolicy {
            poll: Duration::from_millis(400),
            ..stop_policy()
        };
        other_shell
            .verified_cleanup_with(&name("alpha"), &policy, || {
                ran.store(true, Ordering::SeqCst);
                Ok(verified_report())
            })
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    });
    thread::sleep(Duration::from_millis(100));
    drop(first);
    let (mut replacement, events) = RecordingHost::new();
    replacement.wait_ready_result = Err("container lookup failed".into());
    replacement.final_cleanup_failure = Some("removal failed".into());
    let (_sender, shutdown) = Shutdown::channel();
    let failed = runtime.start_with(&alpha, true, false, 0, &replacement, &shutdown);

    assert!(failed.is_err());
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"allocate:alpha".to_owned())
    );
    let error = stop.join().unwrap().unwrap_err();
    assert!(error.contains("another launcher started alpha"), "{error}");
    assert!(!cleaned.load(Ordering::SeqCst));
    assert!(session_files(&runtime).is_empty());
}

#[test]
fn a_stop_cleans_leftovers_older_than_the_launcher_it_stopped() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
    let crashed = Instance {
        name: "alpha".into(),
        version: 1,
        endpoints: endpoints_for(&host_ports(0).unwrap()),
        created_at: None,
        session: Some("crashed-session".into()),
    };
    fs::write(
        runtime.instance_dir(&alpha).join("instance.json"),
        serde_json::to_vec(&crashed).unwrap(),
    )
    .unwrap();
    let (_sender, shutdown) = Shutdown::channel();
    let context = LifecycleContext::new(stop_policy());
    let ownership = runtime.own_start(&alpha, &shutdown, &context).unwrap();
    let cleaned = Arc::new(AtomicBool::new(false));
    let other_shell = Runtime {
        root: runtime.root.clone(),
    };
    let ran = cleaned.clone();
    let stop = thread::spawn(move || {
        other_shell
            .verified_cleanup_with(&name("alpha"), &stop_policy(), || {
                ran.store(true, Ordering::SeqCst);
                Ok(verified_report())
            })
            .map(|report| report.verified())
            .map_err(|error| format!("{error:#}"))
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ownership.session().requested() {
        assert!(Instant::now() < deadline, "stop never requested shutdown");
        thread::sleep(Duration::from_millis(1));
    }
    drop(ownership);

    assert_eq!(stop.join().unwrap(), Ok(true));
    assert!(cleaned.load(Ordering::SeqCst));
}

#[test]
fn a_stop_never_targets_a_session_published_after_its_owner_check() {
    let runtime = runtime_for_tests();
    let alpha = name("alpha");
    let locks = runtime.root.join(".locks");
    let first = runtime.claim_start(&alpha).unwrap();
    let session = locks.join("alpha.session");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&session)
            .status()
            .unwrap()
            .success()
    );
    let cleaned = Arc::new(AtomicBool::new(false));
    let other_shell = Runtime {
        root: runtime.root.clone(),
    };
    let ran = cleaned.clone();
    let stop = thread::spawn(move || {
        let policy = LifecyclePolicy {
            poll: Duration::from_millis(400),
            ..stop_policy()
        };
        other_shell
            .verified_cleanup_with(&name("alpha"), &policy, || {
                ran.store(true, Ordering::SeqCst);
                Ok(verified_report())
            })
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    });
    thread::sleep(Duration::from_millis(200));

    drop(first);
    let replacement = runtime.claim_start(&alpha).unwrap();
    let writer = thread::spawn(move || fs::write(session, "replacement-session"));
    thread::sleep(Duration::from_millis(100));

    assert!(!locks.join("alpha.replacement-session.stop").exists());
    let error = stop.join().unwrap().unwrap_err();
    assert!(error.contains("another launcher started alpha"), "{error}");
    assert!(!cleaned.load(Ordering::SeqCst));
    writer.join().unwrap().unwrap();
    drop(replacement);
}

#[test]
fn start_reports_a_stop_in_progress() {
    let runtime = runtime_for_tests();
    let (inside, entered) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let other_shell = Runtime {
        root: runtime.root.clone(),
    };
    let stop = thread::spawn(move || {
        other_shell
            .verified_cleanup_with(&name("alpha"), &stop_policy(), || {
                inside.send(()).unwrap();
                released.recv().unwrap();
                Ok(verified_report())
            })
            .map(|report| report.verified())
            .map_err(|error| format!("{error:#}"))
    });
    entered.recv().unwrap();
    let (host, events) = RecordingHost::new();
    let (_sender, shutdown) = Shutdown::channel();

    let error = runtime
        .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
        .unwrap_err();

    assert!(error.to_string().contains("is being stopped"), "{error:#}");
    assert!(events.lock().unwrap().is_empty());
    release.send(()).unwrap();
    assert_eq!(stop.join().unwrap(), Ok(true));
    assert!(session_files(&runtime).is_empty());
}

fn owned_leftovers() -> RecordingDocker {
    let mut docker = RecordingDocker::new("ths-alpha-app", "", "");
    docker.inspect(
        "container",
        "ths-alpha-app",
        r#"[{"Id":"owned-app","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
    );
    docker
}

#[test]
fn stop_cleans_what_the_owner_left_behind() {
    let runtime = runtime_for_tests();
    let (owner, _interrupt, events) = spawn_owner(runtime.root.clone(), |host| {
        host.final_cleanup_failure = Some("could not remove ths-alpha-app".into())
    });
    wait_for_event(&events, "wait_for_shutdown:app-1");
    let docker = owned_leftovers();

    let report = stop_after_owner_with(&runtime, &events, "final_cleanup:alpha", &docker).unwrap();

    assert!(report.verified());
    assert_eq!(*docker.runs.lock().unwrap(), ["rm -f owned-app"]);
    let error = owner.join().unwrap().0.unwrap_err();
    assert!(error.contains("could not remove ths-alpha-app"), "{error}");
}

#[test]
fn stop_reports_leftovers_it_cannot_remove() {
    let runtime = runtime_for_tests();
    let (owner, _interrupt, events) = spawn_owner(runtime.root.clone(), |host| {
        host.final_cleanup_failure = Some("could not remove ths-alpha-app".into())
    });
    wait_for_event(&events, "wait_for_shutdown:app-1");
    let docker = owned_leftovers();
    docker.retain.lock().unwrap().push("owned-app".into());

    let report = stop_after_owner_with(&runtime, &events, "final_cleanup:alpha", &docker).unwrap();

    assert!(!report.verified());
    let recovery = report.recovery_path.clone().unwrap();
    assert!(recovery.exists(), "{}", recovery.display());
    let error = format!("{:#}", report.into_result().unwrap_err());
    assert!(error.contains("recovery remains at"), "{error}");
    owner.join().unwrap().0.unwrap_err();
}

#[test]
fn an_owner_marks_its_own_teardown_as_stopping() {
    let runtime = runtime_for_tests();
    let (mut host, _events) = RecordingHost::new();
    host.while_running = Some(Box::new(|| Ended::Interrupted));
    let (_sender, shutdown) = Shutdown::channel();
    let seen = host.stopping_seen.clone();

    runtime
        .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
        .unwrap();

    assert!(seen.load(Ordering::SeqCst));
    assert!(!runtime.root.join(".locks/alpha.stopping").exists());
}
