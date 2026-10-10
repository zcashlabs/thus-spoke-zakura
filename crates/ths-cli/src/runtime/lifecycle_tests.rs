//! Regressions for bounded readiness, recovery, and cleanup.
//!
//! These tests call the shipped runtime functions. Docker-backed cases live in
//! `docker_lifecycle_tests` and stay ignored.

use std::{
    fs,
    io::Write,
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::Result;

use super::readiness::{Probe, probe_once, readiness_client, wait_for_zakura_tip, wait_ready};
use super::recovery::{
    MutationOperation, MutationOutcome, RecoveryJournal, ResourceKind, ResourceRef,
};
use super::*;
use crate::{
    lifecycle::Cancellation,
    test_support::{HttpFixture, HttpMode, short_policy},
};

fn name(value: &str) -> InstanceName {
    value.parse().unwrap()
}

fn cleanup_policy() -> LifecyclePolicy {
    LifecyclePolicy {
        cleanup: Duration::from_secs(2),
        ..short_policy()
    }
}

fn within_attempt(started: Instant, policy: &LifecyclePolicy) {
    assert!(
        started.elapsed() < policy.http_attempt + Duration::from_millis(500),
        "attempt took {:?}",
        started.elapsed()
    );
}

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
        let (sender, receiver) = std::sync::mpsc::channel();
        let shutdown = Shutdown::from_receiver(receiver);
        let requested = || shutdown.try_interrupted();
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                fixture.wait_active(Duration::from_secs(2));
                sender.send(()).unwrap();
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
    let (sender, receiver) = std::sync::mpsc::channel();
    let shutdown = Shutdown::from_receiver(receiver);
    let url = fixture.url();
    let started = Instant::now();
    let sender = sender;
    std::thread::spawn(move || {
        let waiting = Instant::now();
        while waiting.elapsed() < Duration::from_secs(2) && fixture.request_count() == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        sender.send(()).unwrap();
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
    let (_sender, receiver) = std::sync::mpsc::channel();
    let shutdown = Shutdown::from_receiver(receiver);
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
fn success_after_the_phase_deadline_is_still_a_timeout() {
    let policy = short_policy();
    let mut fixture = HttpFixture::new(HttpMode::Health(204));
    let client = readiness_client().unwrap();
    let failure = probe_once(
        &client,
        &fixture.url(),
        Probe::Health,
        Deadline::after(Duration::ZERO),
        Cancellation::Ignore,
        &policy,
    )
    .unwrap_err();
    assert!(format!("{failure:#}").contains("timed out"), "{failure:#}");
    fixture.finish();
}

#[test]
fn failed_or_malformed_inspection_is_not_reported_as_stopped() {
    let policy = short_policy();
    let mut fixture = HttpFixture::new(HttpMode::Health(503));
    let (_sender, receiver) = std::sync::mpsc::channel();
    let shutdown = Shutdown::from_receiver(receiver);
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
    let (_sender, receiver) = std::sync::mpsc::channel();
    let shutdown = Shutdown::from_receiver(receiver);
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
    let (_sender, receiver) = std::sync::mpsc::channel();
    let shutdown = Shutdown::from_receiver(receiver);
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
fn unidentified_create_stays_uncertain_for_either_snapshot() {
    let recorded = ResourceRef {
        kind: ResourceKind::Container,
        name: "ths-alpha-app".into(),
        identity: Some("original-id".into()),
    };
    assert!(recovery::identity_matches(&recorded, "original-id"));
    assert!(!recovery::identity_matches(&recorded, "replacement-id"));
    assert!(!recovery::identity_matches(
        &ResourceRef {
            identity: None,
            ..recorded.clone()
        },
        "original-id"
    ));
    assert!(matches!(
        recovery::reconcile_unidentified_create(true),
        MutationOutcome::Uncertain(_)
    ));
    assert!(matches!(
        recovery::reconcile_unidentified_create(false),
        MutationOutcome::Uncertain(_)
    ));
    let allocated = AllocatedInstance {
        endpoints: Endpoints {
            dashboard: "http://127.0.0.1:1".into(),
            rpc: "http://127.0.0.1:2".into(),
            lightwalletd: "http://127.0.0.1:3".into(),
            p2p: "127.0.0.1:4".into(),
            network: "regtest".into(),
            tls: false,
        },
        resources: vec![recorded],
        app_container_id: "original-id".into(),
        zakura_container_id: "zakura-id".into(),
    };
    assert_eq!(
        allocated.resources[0].identity.as_deref(),
        Some("original-id")
    );
    assert_eq!(allocated.zakura_container_id, "zakura-id");
}

#[test]
fn pending_mutation_survives_without_endpoint_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lifecycle-recovery.json");
    let mut journal = RecoveryJournal::create(path.clone(), &name("alpha")).unwrap();
    journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Container,
                name: "ths-alpha-app".into(),
                identity: None,
            },
            Vec::new(),
            MutationOperation::Create,
        )
        .unwrap();
    let recovered = RecoveryJournal::load(path, &name("alpha"))
        .unwrap()
        .unwrap();
    assert!(matches!(
        recovered.record().mutations[0].outcome,
        MutationOutcome::Pending
    ));
    assert_eq!(
        recovered.record().mutations[0].resource.name,
        "ths-alpha-app"
    );
    assert!(!dir.path().join("instance.json").exists());
}

#[test]
fn mismatched_or_invalid_recovery_refuses_destructive_cleanup() {
    let (runtime, docker, metadata) = owned_cleanup_fixture();
    let path = runtime.recovery_path(&name("alpha"));
    fs::write(&path, b"{not-json").unwrap();
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, false, &cleanup_policy())
        .unwrap();
    assert!(matches!(report.outcome, CleanupOutcome::Uncertain));
    assert!(
        report
            .failures
            .iter()
            .any(|failure| failure.contains("refusing destructive recovery"))
    );
    assert!(docker.removal_calls().is_empty());
    assert!(metadata.exists());

    fs::write(
        &path,
        r#"{"version":9,"instance":"alpha","resources":[],"mutations":[],"failures":[],"unresolved_helpers":[]}"#,
    )
    .unwrap();
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, false, &cleanup_policy())
        .unwrap();
    assert!(matches!(report.outcome, CleanupOutcome::Uncertain));
    assert!(docker.removal_calls().is_empty());

    fs::write(
        &path,
        r#"{"version":1,"instance":"other","resources":[],"mutations":[],"failures":[],"unresolved_helpers":[]}"#,
    )
    .unwrap();
    match RecoveryJournal::load(path, &name("alpha")) {
        Err(error) => assert!(
            error.to_string().contains("refusing destructive recovery"),
            "{error:#}"
        ),
        Ok(_) => panic!("wrong-name recovery journal was accepted"),
    }
}

#[test]
fn failed_rewrite_keeps_the_previous_record_and_skips_the_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(recovery::RECOVERY_FILE);
    let mut journal = RecoveryJournal::create(path.clone(), &name("alpha")).unwrap();
    journal.fail_next_write();
    let docker = StatefulDocker::new();
    let error = ensure_volume_with(
        "ths-alpha-wallet",
        &name("alpha"),
        &docker,
        &mut journal,
        Deadline::after(Duration::from_secs(1)),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("could not be rewritten"),
        "{error:#}"
    );
    assert!(
        docker
            .calls()
            .iter()
            .all(|call| !(call.first().map(String::as_str) == Some("volume")
                && call.get(1).map(String::as_str) == Some("create")))
    );
    let recovered = RecoveryJournal::load(path.clone(), &name("alpha"))
        .unwrap()
        .unwrap();
    assert!(recovered.record().mutations.is_empty());

    let mut journal = RecoveryJournal::load(path.clone(), &name("alpha"))
        .unwrap()
        .unwrap();
    journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Volume,
                name: "ths-alpha-wallet".into(),
                identity: None,
            },
            Vec::new(),
            MutationOperation::Create,
        )
        .unwrap();
    journal.fail_next_write();
    let error = journal
        .finish(
            0,
            MutationOutcome::Acknowledged,
            Some("ths-alpha-wallet".into()),
        )
        .unwrap_err();
    assert!(error.to_string().contains("could not be rewritten"));
    let recovered = RecoveryJournal::load(path, &name("alpha"))
        .unwrap()
        .unwrap();
    assert!(matches!(
        recovered.record().mutations[0].outcome,
        MutationOutcome::Pending
    ));
}

#[test]
fn reloaded_unresolved_helpers_prevent_verified_cleanup() {
    let (runtime, docker, metadata) = owned_cleanup_fixture();
    let instance = name("alpha");
    let mut journal = RecoveryJournal::create(runtime.recovery_path(&instance), &instance).unwrap();
    journal
        .retain_failures(
            vec!["helper was not reaped before cleanup expired".into()],
            vec!["docker container inspect app-id".into()],
        )
        .unwrap();
    drop(journal);

    let report = runtime
        .cleanup_with(&instance, &docker, false, &cleanup_policy())
        .unwrap();

    assert!(!report.verified(), "{report:?}");
    assert!(matches!(report.outcome, CleanupOutcome::Uncertain));
    assert!(metadata.exists());
    let recovered = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
        .unwrap()
        .unwrap();
    assert_eq!(
        recovered.record().unresolved_helpers,
        vec!["docker container inspect app-id"]
    );
}

#[test]
fn uncertain_service_creates_record_every_mounted_dependency() {
    struct LostCreateReply<'a>(&'a StatefulDocker);

    impl DockerResourceCommands for LostCreateReply<'_> {
        fn output(&self, args: &[&str]) -> Result<String> {
            if args.first().copied() == Some("create") {
                anyhow::bail!("timed out waiting for Docker create reply");
            }
            self.0.output(args)
        }

        fn run(&self, args: &[&str]) -> Result<()> {
            self.0.run(args)
        }
    }

    for (service, mounted_volumes) in [
        ("zakura", vec!["ths-alpha-chain", "ths-alpha-config"]),
        ("lightwalletd", vec!["ths-alpha-lightwalletd"]),
        ("app", vec!["ths-alpha-wallet"]),
    ] {
        let (runtime, docker, _) = owned_cleanup_fixture();
        let instance = name("alpha");
        docker
            .resources
            .lock()
            .unwrap()
            .retain(|resource| resource.name != format!("ths-alpha-{service}"));
        let mut journal =
            RecoveryJournal::create(runtime.recovery_path(&instance), &instance).unwrap();
        let network = ResourceRef {
            kind: ResourceKind::Network,
            name: "ths-alpha".into(),
            identity: Some("net-id".into()),
        };
        for resource in
            std::iter::once(network.clone()).chain(mounted_volumes.iter().map(|target| {
                ResourceRef {
                    kind: ResourceKind::Volume,
                    name: (*target).into(),
                    identity: Some((*target).into()),
                }
            }))
        {
            let index = journal
                .begin(resource.clone(), Vec::new(), MutationOperation::Create)
                .unwrap();
            journal
                .finish(index, MutationOutcome::Acknowledged, resource.identity)
                .unwrap();
        }
        let deadline = Deadline::after(short_policy().startup_docker);
        let ports = host_ports(0).unwrap();
        let timeout_docker = LostCreateReply(&docker);
        let dependencies = vec![network];
        let result = match service {
            "zakura" => ensure_zakura(
                "ths-alpha",
                &instance,
                &ports,
                &timeout_docker,
                &mut journal,
                deadline,
                dependencies,
            ),
            "lightwalletd" => ensure_lightwalletd(
                "ths-alpha",
                &instance,
                &ports,
                &timeout_docker,
                &mut journal,
                deadline,
                dependencies,
            ),
            _ => ensure_app(
                "ths-alpha",
                &instance,
                &ports,
                &timeout_docker,
                &mut journal,
                deadline,
                dependencies,
            ),
        };
        assert!(result.is_err());
        let mutation = recovery::unresolved_create(
            journal.record(),
            &ResourceKind::Container,
            &format!("ths-alpha-{service}"),
        )
        .unwrap();
        for target in std::iter::once("ths-alpha").chain(mounted_volumes) {
            assert!(
                mutation
                    .dependencies
                    .iter()
                    .any(|resource| resource.name == target),
                "{service} create omitted dependency {target}: {:?}",
                mutation.dependencies
            );
        }
    }
}

#[test]
fn recorded_volume_identity_does_not_delete_a_foreign_replacement() {
    let (runtime, docker, metadata) = owned_cleanup_fixture();
    let instance = name("alpha");
    let target = "ths-alpha-wallet";
    let mut journal = RecoveryJournal::create(runtime.recovery_path(&instance), &instance).unwrap();
    let index = journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Volume,
                name: target.into(),
                identity: None,
            },
            Vec::new(),
            MutationOperation::Create,
        )
        .unwrap();
    journal
        .finish(index, MutationOutcome::Acknowledged, Some(target.into()))
        .unwrap();
    drop(journal);
    docker.drop_resource(target);
    docker.insert_owned("volume", target, target, "other");

    let report = runtime
        .cleanup_with(&instance, &docker, false, &cleanup_policy())
        .unwrap();

    assert!(
        docker.contains_id(target),
        "foreign replacement was deleted"
    );
    assert!(!report.verified(), "{report:?}");
    assert!(metadata.exists());
    assert!(runtime.recovery_path(&instance).exists());
    assert!(
        docker
            .removal_calls()
            .iter()
            .all(|call| !call.iter().any(|arg| arg == target)),
        "{:?}",
        docker.removal_calls()
    );
}

#[test]
fn recorded_identity_is_removed_without_adopting_a_replacement() {
    let (runtime, docker, _) = owned_cleanup_fixture();
    docker.retain_nothing_but_app_replacement();
    let mut journal =
        RecoveryJournal::create(runtime.recovery_path(&name("alpha")), &name("alpha")).unwrap();
    let index = journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Container,
                name: "ths-alpha-app".into(),
                identity: None,
            },
            Vec::new(),
            MutationOperation::Create,
        )
        .unwrap();
    journal
        .finish(
            index,
            MutationOutcome::Acknowledged,
            Some("original-id".into()),
        )
        .unwrap();
    drop(journal);
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, false, &cleanup_policy())
        .unwrap();
    let removals = docker.removal_calls();
    assert!(
        removals
            .iter()
            .any(|call| call == &vec!["rm".to_owned(), "-f".to_owned(), "original-id".to_owned()]),
        "{removals:?}"
    );
    assert!(
        removals
            .iter()
            .all(|call| !call.iter().any(|arg| arg == "replacement-id")),
        "{removals:?}"
    );
    assert!(!report.verified());
    assert!(docker.contains_id("replacement-id"));
}

#[test]
fn missing_recorded_identity_is_absence_when_the_listing_omits_it() {
    let (runtime, docker, metadata) = owned_cleanup_fixture();
    docker.drop_resource("app-id");
    docker.drop_resource("ths-alpha-chain");
    docker.fail_inspect("missing-app");
    let mut journal =
        RecoveryJournal::create(runtime.recovery_path(&name("alpha")), &name("alpha")).unwrap();
    let container = journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Container,
                name: "ths-alpha-app".into(),
                identity: None,
            },
            Vec::new(),
            MutationOperation::Create,
        )
        .unwrap();
    journal
        .finish(
            container,
            MutationOutcome::Acknowledged,
            Some("missing-app".into()),
        )
        .unwrap();
    let volume = journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Volume,
                name: "ths-alpha-chain".into(),
                identity: None,
            },
            Vec::new(),
            MutationOperation::Create,
        )
        .unwrap();
    journal
        .finish(
            volume,
            MutationOutcome::Acknowledged,
            Some("ths-alpha-chain".into()),
        )
        .unwrap();
    drop(journal);
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, false, &cleanup_policy())
        .unwrap();
    assert!(report.verified(), "{report:?}");
    assert!(!metadata.exists());
    assert!(!runtime.recovery_path(&name("alpha")).exists());
    let removals = docker.removal_calls();
    assert!(
        removals.iter().all(|call| !call
            .iter()
            .any(|arg| arg == "missing-app" || arg == "ths-alpha-chain")),
        "{removals:?}"
    );
}

#[test]
fn failed_inspect_of_a_listed_identity_stays_uncertain() {
    let (runtime, docker, metadata) = owned_cleanup_fixture();
    docker.fail_inspect("app-id");
    let mut journal =
        RecoveryJournal::create(runtime.recovery_path(&name("alpha")), &name("alpha")).unwrap();
    let index = journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Container,
                name: "ths-alpha-app".into(),
                identity: None,
            },
            Vec::new(),
            MutationOperation::Create,
        )
        .unwrap();
    journal
        .finish(index, MutationOutcome::Acknowledged, Some("app-id".into()))
        .unwrap();
    drop(journal);
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, false, &cleanup_policy())
        .unwrap();
    assert!(
        matches!(report.outcome, CleanupOutcome::Uncertain),
        "{report:?}"
    );
    assert!(metadata.exists());
    assert!(docker.contains_id("app-id"));
    assert!(
        docker
            .removal_calls()
            .iter()
            .all(|call| !call.iter().any(|arg| arg == "app-id")),
        "{:?}",
        docker.removal_calls()
    );
}

#[test]
fn hanging_removal_leaves_the_termination_reserve_for_helpers() {
    let mut policy = short_policy();
    policy.cleanup = Duration::from_millis(500);
    policy.termination_reserve = Duration::from_millis(150);
    policy.startup_docker = Duration::from_secs(2);
    let context = LifecycleContext::new(policy);
    let script = hang_on_removal_script();
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: root.path().to_path_buf(),
    };
    let instance = name("alpha");
    fs::create_dir_all(runtime.instance_dir(&instance)).unwrap();
    fs::write(
        runtime.instance_dir(&instance).join("instance.json"),
        b"{}\n",
    )
    .unwrap();
    let mut journal = RecoveryJournal::create(runtime.recovery_path(&instance), &instance).unwrap();
    let index = journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Container,
                name: "ths-alpha-app".into(),
                identity: None,
            },
            Vec::new(),
            MutationOperation::Create,
        )
        .unwrap();
    journal
        .finish(index, MutationOutcome::Acknowledged, Some("app-id".into()))
        .unwrap();
    drop(journal);
    let deadline = Deadline::after(policy.cleanup);
    let mut docker = lifecycle_docker(&context, deadline, Cancellation::Ignore);
    docker.program = "/bin/sh".into();
    docker.prefix_args = vec![script.path().join("docker").to_string_lossy().into_owned()];
    let report = runtime
        .cleanup_core(&instance, &docker, false, deadline, &context)
        .unwrap();
    assert!(
        deadline.remaining() > policy.termination_reserve,
        "cleanup returned with {:?} left, which does not preserve the {:?} helper reserve",
        deadline.remaining(),
        policy.termination_reserve
    );
    assert!(
        report.helpers_finished,
        "hanging removal helper was not reaped inside the cleanup allowance: {report:?}"
    );
    assert!(context.helpers_finished());
}

#[test]
fn acknowledged_removal_without_verified_absence_is_incomplete() {
    let (runtime, docker, metadata) = owned_cleanup_fixture();
    docker.acknowledge_without_removing("ths-alpha-app");
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, false, &cleanup_policy())
        .unwrap();
    assert!(!report.verified());
    assert!(matches!(report.outcome, CleanupOutcome::Incomplete));
    assert!(report.recovery_path.is_some());
    assert!(metadata.exists());
    assert!(
        runtime.recovery_path(&name("alpha")).exists() || metadata.join("instance.json").exists()
    );
}

#[test]
fn fresh_cleanup_budget_ignores_a_latched_shutdown() {
    let (runtime, docker, _) = owned_cleanup_fixture();
    let (sender, receiver) = std::sync::mpsc::channel();
    sender.send(()).unwrap();
    let shutdown = Shutdown::from_receiver(receiver);
    assert!(shutdown.try_interrupted());
    let context = LifecycleContext::new(short_policy());
    let expired = runtime
        .cleanup_core(
            &name("alpha"),
            &docker,
            false,
            Deadline::after(Duration::ZERO),
            &context,
        )
        .unwrap();
    assert!(matches!(expired.outcome, CleanupOutcome::Uncertain));
    assert!(
        docker.removal_calls().is_empty(),
        "{:?}",
        docker.removal_calls()
    );
    let started = Instant::now();
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, false, &cleanup_policy())
        .unwrap();
    assert!(report.verified(), "{report:?}");
    assert!(started.elapsed() < short_policy().cleanup + Duration::from_millis(500));
    let _ = shutdown;
}

#[test]
fn unresolved_create_keeps_its_dependencies() {
    let (runtime, docker, metadata) = owned_cleanup_fixture();
    let mut journal =
        RecoveryJournal::create(runtime.recovery_path(&name("alpha")), &name("alpha")).unwrap();
    journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Container,
                name: "ths-alpha-app".into(),
                identity: None,
            },
            vec![
                ResourceRef {
                    kind: ResourceKind::Volume,
                    name: "ths-alpha-wallet".into(),
                    identity: Some("ths-alpha-wallet".into()),
                },
                ResourceRef {
                    kind: ResourceKind::Network,
                    name: "ths-alpha".into(),
                    identity: Some("net-id".into()),
                },
            ],
            MutationOperation::Create,
        )
        .unwrap();
    drop(journal);
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, true, &cleanup_policy())
        .unwrap();
    assert!(matches!(report.outcome, CleanupOutcome::Uncertain));
    let removals = docker.removal_calls();
    assert!(
        removals.iter().all(|call| !call.iter().any(|arg| {
            arg == "ths-alpha-app"
                || arg == "app-id"
                || arg == "ths-alpha-wallet"
                || arg == "ths-alpha"
                || arg == "net-id"
        })),
        "{removals:?}"
    );
    assert!(
        removals
            .iter()
            .any(|call| call.iter().any(|arg| arg == "ths-alpha-chain"))
    );
    assert!(metadata.exists());
    assert!(runtime.recovery_path(&name("alpha")).exists());
}

#[test]
fn cancelled_image_check_is_not_classified_as_a_missing_image() {
    let policy = short_policy();
    let context = LifecycleContext::new(policy);
    let (sender, receiver) = std::sync::mpsc::channel();
    let shutdown = Shutdown::from_receiver(receiver);
    let requested = || shutdown.try_interrupted();
    let script = sleep_script();
    let docker = LifecycleDocker {
        context: &context,
        enclosing_deadline: None,
        command_cap: policy.startup_docker,
        cancellation: Cancellation::Observe(&requested),
        program: "/bin/sh".into(),
        prefix_args: vec![script.path().join("docker").to_string_lossy().into_owned()],
        env: Vec::new(),
    };
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        sender.send(()).unwrap();
    });
    let started = Instant::now();
    let error = startup_require_image(&docker, "example/image:missing").unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("cancelled") || text.contains("timed out"),
        "{text}"
    );
    assert!(!text.contains("ths pull"), "{text}");
    assert!(!text.contains("unavailable"), "{text}");
    assert!(started.elapsed() < policy.startup_docker + Duration::from_millis(500));
    let _ = context.finish_helpers(Deadline::after(policy.startup_docker));
}

#[test]
fn missing_image_advice_requires_a_completed_listing() {
    let policy = short_policy();
    let context = LifecycleContext::new(policy);
    let script = image_listing_script(false);
    let docker = script_docker(&script, &context, policy.startup_docker);
    let error = startup_require_image(&docker, "example/image:missing").unwrap_err();
    assert!(error.to_string().contains("ths pull"), "{error:#}");

    let present = image_listing_script(true);
    let docker = script_docker(&present, &context, policy.startup_docker);
    startup_require_image(&docker, "example/image:missing")
        .expect("a completed listing that contains the image is not a pull failure");

    let sleep = sleep_script();
    let docker = LifecycleDocker {
        context: &context,
        enclosing_deadline: None,
        command_cap: policy.startup_docker,
        cancellation: Cancellation::Ignore,
        program: "/bin/sh".into(),
        prefix_args: vec![sleep.path().join("docker").to_string_lossy().into_owned()],
        env: Vec::new(),
    };
    let started = Instant::now();
    let error = startup_require_image(&docker, "example/image:missing").unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("timed out"), "{text}");
    assert!(!text.contains("ths pull"), "{text}");
    assert!(started.elapsed() < policy.startup_docker + Duration::from_millis(500));
    let _ = context.finish_helpers(Deadline::after(policy.startup_docker));
}

struct RunningDocker;

#[test]
fn mismatched_volume_identity_refuses_destructive_recovery() {
    let (runtime, docker, metadata) = owned_cleanup_fixture();
    let instance = name("alpha");
    let mut journal = RecoveryJournal::create(runtime.recovery_path(&instance), &instance).unwrap();
    let index = journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Volume,
                name: "ths-alpha-lightwalletd".into(),
                identity: None,
            },
            Vec::new(),
            MutationOperation::Create,
        )
        .unwrap();
    journal
        .finish(index, MutationOutcome::Acknowledged, Some("lw-id".into()))
        .unwrap();
    drop(journal);
    let report = runtime
        .cleanup_with(&instance, &docker, false, &cleanup_policy())
        .unwrap();
    assert!(!report.verified(), "{report:?}");
    assert!(docker.removal_calls().is_empty());
    assert!(metadata.exists());
}

#[test]
fn same_name_container_and_volume_are_both_removed() {
    let (runtime, docker, metadata) = owned_cleanup_fixture();
    let instance = name("alpha");
    let target = "ths-alpha-lightwalletd";
    let mut journal = RecoveryJournal::create(runtime.recovery_path(&instance), &instance).unwrap();
    for (kind, id) in [
        (ResourceKind::Volume, target),
        (ResourceKind::Container, "lw-id"),
    ] {
        let index = journal
            .begin(
                ResourceRef {
                    kind,
                    name: target.into(),
                    identity: None,
                },
                Vec::new(),
                MutationOperation::Create,
            )
            .unwrap();
        journal
            .finish(index, MutationOutcome::Acknowledged, Some(id.into()))
            .unwrap();
    }
    drop(journal);
    let report = runtime
        .cleanup_with(&instance, &docker, false, &cleanup_policy())
        .unwrap();
    assert!(report.verified(), "{report:?}");
    assert!(
        !docker.contains_id(target),
        "lightwalletd volume was left behind"
    );
    assert!(!docker.contains_id("lw-id"));
    assert!(!metadata.exists());
}

#[test]
fn uncertain_start_preserves_the_creation_dependencies() {
    let (runtime, docker, _) = owned_cleanup_fixture();
    let instance = name("alpha");
    let mut journal = RecoveryJournal::create(runtime.recovery_path(&instance), &instance).unwrap();
    let dependencies = vec![
        ResourceRef {
            kind: ResourceKind::Network,
            name: "ths-alpha".into(),
            identity: Some("net-id".into()),
        },
        ResourceRef {
            kind: ResourceKind::Volume,
            name: "ths-alpha-wallet".into(),
            identity: Some("ths-alpha-wallet".into()),
        },
    ];
    let index = journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Container,
                name: "ths-alpha-app".into(),
                identity: None,
            },
            dependencies,
            MutationOperation::Create,
        )
        .unwrap();
    journal
        .finish(index, MutationOutcome::Acknowledged, Some("app-id".into()))
        .unwrap();
    assert!(
        start_container(
            "app-id",
            "ths-alpha-app",
            &docker,
            &mut journal,
            Deadline::after(Duration::from_secs(1))
        )
        .is_err()
    );
    let mutation = journal.record().mutations.last().unwrap();
    assert_eq!(mutation.operation, MutationOperation::Start);
    let names: Vec<_> = mutation
        .dependencies
        .iter()
        .map(|resource| resource.name.as_str())
        .collect();
    assert_eq!(names, ["ths-alpha", "ths-alpha-wallet"]);
    drop(journal);
    let report = runtime
        .cleanup_with(&instance, &docker, true, &cleanup_policy())
        .unwrap();
    assert!(!report.verified());
    assert!(docker.contains_id("app-id"));
    assert!(docker.contains_id("net-id"));
    assert!(docker.contains_id("ths-alpha-wallet"));
}

#[test]
fn cleanup_of_a_legacy_instance_records_failed_removal() {
    let (runtime, docker, metadata) = owned_cleanup_fixture();
    docker.acknowledge_without_removing("ths-alpha-wallet");
    let instance = name("alpha");
    let report = runtime
        .cleanup_with(&instance, &docker, false, &cleanup_policy())
        .unwrap();
    assert!(!report.verified());
    assert!(metadata.exists());
    let journal = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
        .unwrap()
        .unwrap();
    assert!(
        journal
            .record()
            .mutations
            .iter()
            .any(|mutation| mutation.resource.name == "ths-alpha-wallet"
                && mutation.operation == MutationOperation::Remove)
    );
}

#[test]
fn cleanup_journal_write_failure_is_reported_and_retained() {
    struct FailedJournalWrite<'a> {
        docker: &'a StatefulDocker,
        temporary: std::path::PathBuf,
    }
    impl DockerResourceCommands for FailedJournalWrite<'_> {
        fn output(&self, args: &[&str]) -> Result<String> {
            self.docker.output(args)
        }
        fn run(&self, args: &[&str]) -> Result<()> {
            self.docker.run(args)?;
            if args.first() == Some(&"network") && !self.temporary.exists() {
                fs::create_dir(&self.temporary)?;
            }
            Ok(())
        }
    }
    let (runtime, docker, metadata) = owned_cleanup_fixture();
    let instance = name("alpha");
    let path = runtime.recovery_path(&instance);
    RecoveryJournal::create(path.clone(), &instance).unwrap();
    let failed = FailedJournalWrite {
        docker: &docker,
        temporary: path.with_extension("json.tmp"),
    };
    let report = runtime
        .cleanup_with(&instance, &failed, false, &cleanup_policy())
        .unwrap();
    assert!(!report.verified(), "{report:?}");
    assert!(metadata.exists());
    assert!(path.exists());
    assert!(
        report
            .failures
            .iter()
            .any(|failure| failure.contains("writing")),
        "{report:?}"
    );
}

impl DockerResourceCommands for RunningDocker {
    fn output(&self, args: &[&str]) -> Result<String> {
        match args {
            ["container", "inspect", "--format", "{{.State.Running}}", _] => Ok("true".into()),
            ["logs", "--tail", "50", _] => Ok("still starting".into()),
            _ => anyhow::bail!("unexpected docker read {args:?}"),
        }
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        anyhow::bail!("unexpected docker run {args:?}")
    }
}

struct ScriptedDocker {
    running: Result<String, String>,
    logs: Result<String, String>,
}

impl DockerResourceCommands for ScriptedDocker {
    fn output(&self, args: &[&str]) -> Result<String> {
        match args {
            ["container", "inspect", "--format", "{{.State.Running}}", _] => {
                self.running.clone().map_err(anyhow::Error::msg)
            }
            ["logs", "--tail", "50", _] => self.logs.clone().map_err(anyhow::Error::msg),
            _ => anyhow::bail!("unexpected docker read {args:?}"),
        }
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        anyhow::bail!("unexpected docker run {args:?}")
    }
}

struct FakeResource {
    kind: String,
    name: String,
    id: String,
    body: String,
}

struct StatefulDocker {
    root: tempfile::TempDir,
    resources: Mutex<Vec<FakeResource>>,
    calls: Mutex<Vec<Vec<String>>>,
    retain: Mutex<Vec<String>>,
    inspect_errors: Mutex<Vec<String>>,
}

impl StatefulDocker {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            resources: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            retain: Mutex::new(Vec::new()),
            inspect_errors: Mutex::new(Vec::new()),
        }
    }

    fn insert_owned(&self, kind: &str, name: &str, id: &str, instance: &str) {
        let labels = serde_json::json!({ INSTANCE_LABEL: instance });
        let item = if kind == "container" {
            serde_json::json!({"Id": id, "Name": name, "Config": {"Labels": labels.clone()}, "Labels": labels})
        } else if kind == "volume" {
            serde_json::json!({"Name": name, "Labels": labels})
        } else {
            serde_json::json!({"Id": id, "Name": name, "Labels": labels})
        };
        self.resources.lock().unwrap().push(FakeResource {
            kind: kind.to_owned(),
            name: name.to_owned(),
            id: id.to_owned(),
            body: serde_json::to_string(&serde_json::Value::Array(vec![item])).unwrap(),
        });
    }

    fn acknowledge_without_removing(&self, target: &str) {
        self.retain.lock().unwrap().push(target.to_owned());
    }

    fn drop_resource(&self, id: &str) {
        self.resources
            .lock()
            .unwrap()
            .retain(|resource| resource.id != id);
    }

    fn fail_inspect(&self, id: &str) {
        self.inspect_errors.lock().unwrap().push(id.to_owned());
    }

    fn retain_nothing_but_app_replacement(&self) {
        self.resources.lock().unwrap().clear();
        self.insert_owned("container", "ths-alpha-app", "original-id", "alpha");
        self.insert_owned("container", "ths-alpha-app", "replacement-id", "alpha");
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    fn removal_calls(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|call| {
                matches!(
                    call.as_slice(),
                    [first, second, ..] if (*first == "rm" && second == "-f")
                        || (*first == "volume" && second == "rm")
                        || (*first == "network" && second == "rm")
                )
            })
            .collect()
    }

    fn contains_id(&self, id: &str) -> bool {
        self.resources
            .lock()
            .unwrap()
            .iter()
            .any(|resource| resource.id == id)
    }

    fn record(&self, args: &[&str]) {
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| (*arg).to_owned()).collect());
    }

    fn names(&self, kind: &str) -> String {
        self.resources
            .lock()
            .unwrap()
            .iter()
            .filter(|resource| resource.kind == kind)
            .map(|resource| resource.name.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn ids(&self, kind: &str) -> String {
        self.resources
            .lock()
            .unwrap()
            .iter()
            .filter(|resource| resource.kind == kind)
            .map(|resource| resource.id.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl DockerResourceCommands for StatefulDocker {
    fn output(&self, args: &[&str]) -> Result<String> {
        self.record(args);
        match args {
            ["container", "ls", "-a", "--format", "{{.Names}}"] => Ok(self.names("container")),
            ["container", "ls", "-a", "--no-trunc", "--format", "{{.ID}}"] => {
                Ok(self.ids("container"))
            }
            ["volume", "ls", "--format", "{{.Name}}"] => Ok(self.names("volume")),
            ["network", "ls", "--format", "{{.Name}}"] => Ok(self.names("network")),
            ["network", "ls", "--no-trunc", "--format", "{{.ID}}"] => Ok(self.ids("network")),
            [kind, "inspect", target] => {
                if self
                    .inspect_errors
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|id| id == target)
                {
                    anyhow::bail!("No such {kind}: {target}");
                }
                let resources = self.resources.lock().unwrap();
                if let Some(resource) = resources
                    .iter()
                    .find(|resource| resource.kind == *kind && resource.id == *target)
                {
                    return Ok(resource.body.clone());
                }
                let matches: Vec<_> = resources
                    .iter()
                    .filter(|resource| resource.kind == *kind && resource.name == *target)
                    .collect();
                if matches.len() == 1 {
                    Ok(matches[0].body.clone())
                } else if matches.is_empty() {
                    Ok(String::new())
                } else {
                    anyhow::bail!("multiple {kind} resources named {target}")
                }
            }
            _ => anyhow::bail!("unexpected docker read {args:?}"),
        }
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        self.record(args);
        let (kind, id) = match args {
            ["rm", "-f", id] => ("container", *id),
            ["volume", "rm", id] => ("volume", *id),
            ["network", "rm", id] => ("network", *id),
            ["volume", "create", "--label", _, volume] => {
                self.insert_owned("volume", volume, volume, "alpha");
                return Ok(());
            }
            _ => anyhow::bail!("unexpected docker run {args:?}"),
        };
        let retain = self.retain.lock().unwrap().clone();
        let retained = retain.iter().any(|target| target == id)
            || self.resources.lock().unwrap().iter().any(|resource| {
                retain.iter().any(|target| target == &resource.name)
                    && resource.kind == kind
                    && (resource.id == id || resource.name == id)
            });
        if retained {
            return Ok(());
        }
        self.resources.lock().unwrap().retain(|resource| {
            !(resource.kind == kind && (resource.id == id || resource.name == id))
        });
        Ok(())
    }
}

fn owned_cleanup_fixture() -> (Runtime, StatefulDocker, std::path::PathBuf) {
    let docker = StatefulDocker::new();
    let runtime = Runtime {
        root: docker.root.path().to_path_buf(),
    };
    let instance = name("alpha");
    fs::create_dir_all(runtime.instance_dir(&instance)).unwrap();
    fs::write(
        runtime.instance_dir(&instance).join("instance.json"),
        b"{}\n",
    )
    .unwrap();
    for (kind, target, id) in [
        ("container", "ths-alpha-app", "app-id"),
        ("container", "ths-alpha-lightwalletd", "lw-id"),
        ("container", "ths-alpha-zakura", "zakura-id"),
        ("container", "ths-alpha-init", "init-id"),
        ("volume", "ths-alpha-chain", "ths-alpha-chain"),
        ("volume", "ths-alpha-wallet", "ths-alpha-wallet"),
        ("volume", "ths-alpha-lightwalletd", "ths-alpha-lightwalletd"),
        ("volume", "ths-alpha-config", "ths-alpha-config"),
        ("network", "ths-alpha", "net-id"),
    ] {
        docker.insert_owned(kind, target, id, "alpha");
    }
    let metadata = runtime.instance_dir(&instance);
    (runtime, docker, metadata)
}

fn script_docker<'a>(
    script: &tempfile::TempDir,
    context: &'a LifecycleContext,
    cap: Duration,
) -> LifecycleDocker<'a> {
    LifecycleDocker {
        context,
        enclosing_deadline: None,
        command_cap: cap,
        cancellation: Cancellation::Ignore,
        program: "/bin/sh".into(),
        prefix_args: vec![script.path().join("docker").to_string_lossy().into_owned()],
        env: Vec::new(),
    }
}

fn hang_on_removal_script() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("docker");
    let mut file = fs::File::create(&path).unwrap();
    write!(
        file,
        "#!/bin/sh\nif [ \"$1\" = \"rm\" ]; then\n  sleep 30\nfi\nif [ \"$1\" = \"volume\" ] && [ \"$2\" = \"rm\" ]; then\n  sleep 30\nfi\nif [ \"$1\" = \"network\" ] && [ \"$2\" = \"rm\" ]; then\n  sleep 30\nfi\nif [ \"$1\" = \"container\" ] && [ \"$2\" = \"inspect\" ]; then\n  printf '%s\\n' '[{{\"Id\":\"app-id\",\"Config\":{{\"Labels\":{{\"com.zakura.ths.instance\":\"alpha\"}}}}}}]'\n  exit 0\nfi\nexit 0\n"
    )
    .unwrap();
    make_executable(&path);
    dir
}

fn sleep_script() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("docker");
    fs::write(&path, "#!/bin/sh\nsleep 30\n").unwrap();
    make_executable(&path);
    dir
}

fn hanging_log_script() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("docker");
    let mut file = fs::File::create(&path).unwrap();
    write!(
        file,
        "#!/bin/sh\nif [ \"$1\" = \"logs\" ]; then\n  sleep 30\nfi\nprintf '%s\\n' false\n"
    )
    .unwrap();
    make_executable(&path);
    dir
}

fn image_listing_script(present: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("docker");
    let listing = if present {
        "example/image:missing"
    } else {
        "other:tag"
    };
    let mut file = fs::File::create(&path).unwrap();
    write!(
        file,
        "#!/bin/sh\nif [ \"$2\" = \"inspect\" ]; then\n  echo 'No such image' >&2\n  exit 1\nfi\necho '{listing}'\n"
    )
    .unwrap();
    make_executable(&path);
    dir
}

fn make_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}
