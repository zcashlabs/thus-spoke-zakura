use super::*;

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

/// A child that never reports itself exited, regardless of what actually
/// happens to the real process. This lets a test deterministically force
/// "the removal looks hung" without racing a real process's actual exit
/// timing against the kill signal and the OS's scheduling of that signal's
/// delivery — both of which become unpredictable under heavy contention.
/// `kill` still signals the real child, so nothing is left running.
struct NeverExits;

impl ChildOps for NeverExits {
    fn kill(&self, child: &mut std::process::Child) -> std::io::Result<()> {
        child.kill()
    }

    fn try_wait(
        &self,
        _child: &mut std::process::Child,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        Ok(None)
    }
}

/// Fakes every ownership/absence read deterministically and instantly, and
/// delegates removal (`run()`) to a real `LifecycleDocker` so the thing
/// actually under test — a hanging removal and its reaping — stays real.
/// This is the `DockerResourceCommands` seam, one level above
/// `LifecycleDocker`/`ChildOps`: reads never spawn a subprocess at all, so
/// neither fork/exec scheduling nor a real read's own tail latency can
/// compete with the removal for the termination reserve. Responses below are
/// exactly the reads `cleanup_core` issues for this fixture's one tracked
/// container (confirmed by tracing `consider`/`resource_absent` for a
/// resource whose recorded mutation outcome is `Acknowledged`, not
/// `Pending`/`Uncertain`, so it takes the `resource_absent`, not
/// `recorded_identity_absent`, path); anything else is a test bug, not a
/// case to paper over.
struct FakeReadsRealRemoval<'a> {
    removal: LifecycleDocker<'a>,
}

impl DockerResourceCommands for FakeReadsRealRemoval<'_> {
    fn output(&self, args: &[&str]) -> Result<String> {
        match args {
            ["container", "inspect", "app-id"] => Ok(
                r#"[{"Id":"app-id","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#
                    .to_owned(),
            ),
            ["container", "ls", "-a", "--format", "{{.Names}}"]
            | ["volume", "ls", "--format", "{{.Name}}"]
            | ["network", "ls", "--format", "{{.Name}}"] => Ok(String::new()),
            _ => anyhow::bail!("unexpected docker read in hanging-removal test: {args:?}"),
        }
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        self.removal.run(args)
    }
}

// This splits into two independent checks rather than one wall-clock
// assertion over an entire real run, per review feedback:
//
// 1. Deterministic reserve accounting: the removal docker's own
//    `operation_deadline` must sit at least `termination_reserve` before the
//    overall deadline. This compares two already-computed `Deadline`s
//    (frozen `Instant`s) with no real waiting involved, so it cannot be
//    affected by subprocess scheduling, fsync latency, or any other real
//    timing noise — it is exactly as deterministic under parallel load as
//    it is running alone.
// 2. Real removal and helper-reaping coverage, with a generous timing
//    allowance: `NeverExits` (above) still forces "looks hung"
//    deterministically rather than racing a real process's kill-to-exit
//    timing, and `FakeReadsRealRemoval` (above) keeps every read
//    deterministic and subprocess-free too. But this phase no longer
//    asserts on exactly how much wall-clock margin survived — only that
//    removal genuinely started (`removal_marker`) and reaping genuinely
//    succeeded (`helpers_finished`) — because that margin is downstream of
//    real OS scheduling (thread wake-ups, fsync, process reaping) that no
//    budget can be proven sufficient against under unbounded contention.
#[test]
fn hanging_removal_leaves_the_termination_reserve_for_helpers() {
    let never_exits = NeverExits;
    let mut policy = short_policy();
    policy.cleanup = Duration::from_secs(3);
    policy.termination_reserve = Duration::from_millis(800);
    policy.startup_docker = Duration::from_secs(3);
    policy.poll = Duration::from_millis(20);
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
    let removal_marker = root.path().join("removal-started");
    let deadline = Deadline::after(policy.cleanup);
    let mut removal =
        lifecycle_docker(&context, deadline, Cancellation::Ignore).with_child_ops(&never_exits);
    removal.program = "/bin/sh".into();
    removal.prefix_args = vec![script.path().join("docker").to_string_lossy().into_owned()];
    removal.env.push((
        "THS_REMOVAL_MARKER".to_owned(),
        removal_marker.to_string_lossy().into_owned(),
    ));

    // 1. Deterministic reserve accounting. No real time has elapsed since
    // `deadline` and `removal`'s `operation_deadline` were computed — both
    // are frozen `Instant`s — so this comparison cannot be perturbed by
    // scheduling or I/O, under any amount of parallel load.
    let operation_deadline = removal.operation_deadline();
    assert!(
        deadline.0.saturating_duration_since(operation_deadline.0) >= policy.termination_reserve,
        "removal's operation deadline {:?} does not reserve {:?} before the overall deadline {:?}",
        operation_deadline,
        policy.termination_reserve,
        deadline
    );

    // 2. Real removal and helper-reaping coverage, with a generous timing
    // allowance (`policy.cleanup` above): only whether removal started and
    // reaping succeeded is asserted, not how much margin was left over.
    let docker = FakeReadsRealRemoval { removal };
    let report = runtime
        .cleanup_core(&instance, &docker, false, deadline, &context)
        .unwrap();
    assert!(
        removal_marker.exists(),
        "rm -f app-id was never issued; this exercised something other than a hanging removal"
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
    let (sender, shutdown) = Shutdown::channel();
    sender.send(Ended::Interrupted).unwrap();
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
    // Classification, not timing: a fresh attempt must not inherit the prior
    // attempt's Uncertain outcome regardless of how long persistence or
    // scheduling happens to take, so this uses an effectively unbounded
    // budget rather than racing a real deadline.
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, false, &cleanup_policy())
        .unwrap();
    assert!(report.verified(), "{report:?}");
    let _ = shutdown;
}

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
