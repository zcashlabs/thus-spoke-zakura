use super::*;

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
        app_container: WatchedContainer {
            id: "original-id".into(),
            created: "2026-01-01T00:00:00Z".into(),
        },
        zakura_container_id: "zakura-id".into(),
    };
    assert_eq!(
        allocated.resources[0].identity.as_deref(),
        Some("original-id")
    );
    assert_eq!(allocated.zakura_container_id, "zakura-id");
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
fn uncertain_service_creates_record_every_mounted_dependency() {
    struct LostCreateReply<'a>(&'a StatefulDocker);

    impl DockerResourceCommands for LostCreateReply<'_> {
        fn output(&self, args: &[&str]) -> Result<String> {
            if args.first().copied() == Some("create") {
                if args.contains(&ZAKURA_IMAGE) {
                    for pair in [
                        ["-v", "ths-alpha-chain:/data"],
                        ["-e", "ZAKURA_STATE__CACHE_DIR=/data"],
                    ] {
                        assert!(
                            args.windows(2).any(|arguments| arguments == pair),
                            "{args:?}"
                        );
                    }
                }
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
                MiningConfig::default(),
            )
            .map(|app| app.id),
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

fn failed_start_fixture(outcome: MutationOutcome) -> (Runtime, StatefulDocker, ResourceRef) {
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
    for resource in &dependencies {
        let index = journal
            .begin(resource.clone(), Vec::new(), MutationOperation::Create)
            .unwrap();
        journal
            .finish(
                index,
                MutationOutcome::Acknowledged,
                resource.identity.clone(),
            )
            .unwrap();
    }
    let container = ResourceRef {
        kind: ResourceKind::Container,
        name: "ths-alpha-app".into(),
        identity: Some("app-id".into()),
    };
    let index = journal
        .begin(
            container.clone(),
            dependencies.clone(),
            MutationOperation::Create,
        )
        .unwrap();
    journal
        .finish(
            index,
            MutationOutcome::Acknowledged,
            container.identity.clone(),
        )
        .unwrap();
    if outcome == MutationOutcome::Pending {
        journal
            .begin(container.clone(), dependencies, MutationOperation::Start)
            .unwrap();
    } else {
        let error = start_container(
            "app-id",
            &container.name,
            &docker,
            &mut journal,
            Deadline::after(Duration::from_secs(1)),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("unexpected docker run"),
            "{error:#}"
        );
    }
    drop(journal);
    (runtime, docker, container)
}

struct CreateAppDocker<'a> {
    docker: &'a StatefulDocker,
    id: &'a str,
    fail_inspect: bool,
}

impl DockerResourceCommands for CreateAppDocker<'_> {
    fn output(&self, args: &[&str]) -> Result<String> {
        if args
            == [
                "create",
                "--name",
                "ths-alpha-app",
                "--label",
                "com.zakura.ths.instance=alpha",
                ZAKURA_IMAGE,
            ]
        {
            self.docker.record(args);
            self.docker
                .insert_owned("container", "ths-alpha-app", self.id, "alpha");
            return Ok(self.id.to_owned());
        }
        if self.fail_inspect && args == ["container", "inspect", self.id] {
            self.docker.record(args);
            anyhow::bail!("temporary Docker inspection failure");
        }
        self.docker.output(args)
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        self.docker.run(args)
    }
}

fn create_test_app(
    runtime: &Runtime,
    docker: &StatefulDocker,
    id: &str,
    fail_inspect: bool,
) -> Result<String> {
    let instance = name("alpha");
    let mut journal = RecoveryJournal::create(runtime.recovery_path(&instance), &instance)?;
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
    for dependency in &dependencies {
        let index = journal.begin(dependency.clone(), Vec::new(), MutationOperation::Create)?;
        journal.finish(
            index,
            MutationOutcome::Acknowledged,
            dependency.identity.clone(),
        )?;
    }
    create_container(
        "ths-alpha-app",
        &instance,
        &CreateAppDocker {
            docker,
            id,
            fail_inspect,
        },
        &mut journal,
        Deadline::after(Duration::from_secs(1)),
        dependencies,
        &[
            "create",
            "--name",
            "ths-alpha-app",
            "--label",
            "com.zakura.ths.instance=alpha",
            ZAKURA_IMAGE,
        ],
    )
}

fn failed_create_fixture() -> (Runtime, StatefulDocker, ResourceRef) {
    let (runtime, docker, _) = owned_cleanup_fixture();
    docker.drop_resource("app-id");
    let error = create_test_app(&runtime, &docker, "app-id", true).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("temporary Docker inspection failure"),
        "{error:#}"
    );
    let journal = RecoveryJournal::load(runtime.recovery_path(&name("alpha")), &name("alpha"))
        .unwrap()
        .unwrap();
    let mutation = journal.record().mutations.last().unwrap();
    assert_eq!(mutation.operation, MutationOperation::Create);
    assert_eq!(mutation.resource.identity.as_deref(), Some("app-id"));
    assert!(matches!(mutation.outcome, MutationOutcome::Uncertain(_)));
    let container = mutation.resource.clone();
    (runtime, docker, container)
}

#[test]
fn failed_create_recovers_after_inspection_failure() {
    for partial in [true, false] {
        let (runtime, docker, _) = failed_create_fixture();
        let instance = name("alpha");
        let report = runtime
            .cleanup_with(&instance, &docker, partial, &cleanup_policy())
            .unwrap();
        assert!(report.verified(), "{report:?}");
        assert!(docker.resources.lock().unwrap().is_empty());
        assert!(!runtime.instance_dir(&instance).exists());
        assert_container_removed_before_dependencies(&docker);
        docker.insert_owned("network", "ths-alpha", "net-id", "alpha");
        docker.insert_owned("volume", "ths-alpha-wallet", "ths-alpha-wallet", "alpha");
        assert_eq!(
            create_test_app(&runtime, &docker, "fresh-app-id", false).unwrap(),
            "fresh-app-id"
        );
        let report = runtime
            .cleanup_with(&instance, &docker, partial, &cleanup_policy())
            .unwrap();
        assert!(report.verified(), "{report:?}");
        assert!(docker.resources.lock().unwrap().is_empty());
        assert!(!runtime.instance_dir(&instance).exists());
    }
}

#[test]
fn failed_create_retries_cleanup_after_docker_recovers() {
    let (runtime, docker, _) = failed_create_fixture();
    docker.fail_inspect("app-id");
    let instance = name("alpha");
    let report = runtime
        .cleanup_with(&instance, &docker, true, &cleanup_policy())
        .unwrap();
    assert!(!report.verified(), "{report:?}");
    assert!(docker.contains_id("app-id"));
    assert_allocation_dependencies_kept(&runtime, &docker);
    docker.inspect_errors.lock().unwrap().clear();
    let report = runtime
        .cleanup_with(&instance, &docker, false, &cleanup_policy())
        .unwrap();
    assert!(report.verified(), "{report:?}");
    assert!(docker.resources.lock().unwrap().is_empty());
    assert!(!runtime.instance_dir(&instance).exists());
}

#[test]
fn failed_create_keeps_dependencies_when_cleanup_cannot_verify_ownership_or_absence() {
    for failure in [
        "inspection",
        "removal",
        "foreign ownership",
        "conflicting create",
        "conflicting start",
        "replacement",
    ] {
        let (runtime, docker, container) = failed_create_fixture();
        let instance = name("alpha");
        match failure {
            "inspection" => docker.fail_inspect("app-id"),
            "removal" => docker.acknowledge_without_removing("app-id"),
            "foreign ownership" => {
                docker.drop_resource("app-id");
                docker.insert_owned("container", "ths-alpha-app", "app-id", "other");
            }
            "conflicting create" | "conflicting start" => {
                let mut journal =
                    RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
                        .unwrap()
                        .unwrap();
                let dependencies = journal
                    .record()
                    .mutations
                    .last()
                    .unwrap()
                    .dependencies
                    .clone();
                let mut conflicting = container;
                conflicting.identity = Some("other-id".into());
                journal
                    .begin(
                        conflicting,
                        dependencies,
                        if failure == "conflicting create" {
                            MutationOperation::Create
                        } else {
                            MutationOperation::Start
                        },
                    )
                    .unwrap();
            }
            "replacement" => {
                docker.drop_resource("app-id");
                docker.insert_owned("container", "ths-alpha-app", "replacement-id", "alpha");
            }
            _ => unreachable!(),
        }
        let report = runtime
            .cleanup_with(&instance, &docker, true, &cleanup_policy())
            .unwrap();
        assert!(!report.verified(), "{failure}: {report:?}");
        assert_allocation_dependencies_kept(&runtime, &docker);
        if failure == "replacement" {
            assert!(docker.contains_id("replacement-id"));
            assert!(!docker.removal_calls().iter().any(|call| {
                call.iter()
                    .any(|arg| arg == "replacement-id" || arg == "ths-alpha-app")
            }));
        } else {
            assert!(docker.contains_id("app-id"));
        }
    }
}

#[test]
fn failed_create_reconciles_an_already_missing_container() {
    for interrupted_removal in [false, true] {
        let (runtime, docker, container) = failed_create_fixture();
        let instance = name("alpha");
        if interrupted_removal {
            let mut journal = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
                .unwrap()
                .unwrap();
            let index = journal
                .begin(container, Vec::new(), MutationOperation::Remove)
                .unwrap();
            journal
                .finish(
                    index,
                    MutationOutcome::ReconciledAbsent,
                    Some("app-id".into()),
                )
                .unwrap();
        }
        docker.drop_resource("app-id");
        let report = runtime
            .cleanup_with(&instance, &docker, false, &cleanup_policy())
            .unwrap();
        assert!(report.verified(), "{report:?}");
        assert!(docker.resources.lock().unwrap().is_empty());
        assert!(!runtime.instance_dir(&instance).exists());
        assert!(
            !docker
                .removal_calls()
                .iter()
                .any(|call| call == &["rm", "-f", "app-id"])
        );
    }
}

#[test]
fn resolved_create_preserves_shared_unidentified_create_dependencies_and_diagnostics() {
    struct LostRemoval<'a> {
        docker: &'a StatefulDocker,
        lost_reply: bool,
    }
    impl DockerResourceCommands for LostRemoval<'_> {
        fn output(&self, args: &[&str]) -> Result<String> {
            self.docker.output(args)
        }
        fn run(&self, args: &[&str]) -> Result<()> {
            self.docker.run(args)?;
            if self.lost_reply && args == ["rm", "-f", "app-id"] {
                anyhow::bail!("lost app removal reply");
            }
            Ok(())
        }
    }
    for lost_reply in [false, true] {
        let (runtime, docker, _) = failed_create_fixture();
        let instance = name("alpha");
        let mut journal = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
            .unwrap()
            .unwrap();
        let dependencies = journal
            .record()
            .mutations
            .last()
            .unwrap()
            .dependencies
            .clone();
        journal
            .begin(
                ResourceRef {
                    kind: ResourceKind::Container,
                    name: "ths-alpha-init".into(),
                    identity: None,
                },
                dependencies,
                MutationOperation::Create,
            )
            .unwrap();
        drop(journal);
        let report = runtime
            .cleanup_with(
                &instance,
                &LostRemoval {
                    docker: &docker,
                    lost_reply,
                },
                true,
                &cleanup_policy(),
            )
            .unwrap();
        assert!(!report.verified(), "{report:?}");
        assert!(!docker.contains_id("app-id"));
        assert!(docker.contains_id("init-id"));
        assert_allocation_dependencies_kept(&runtime, &docker);
        let journal = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
            .unwrap()
            .unwrap();
        assert!(
            journal
                .record()
                .mutations
                .iter()
                .any(|mutation| mutation.operation == MutationOperation::Create
                    && mutation.resource.identity.as_deref() == Some("app-id")
                    && mutation.outcome == MutationOutcome::ReconciledAbsent)
        );
        assert!(
            journal
                .record()
                .failures
                .iter()
                .any(|failure| failure.contains("temporary Docker inspection failure"))
        );
        if lost_reply {
            assert!(
                journal
                    .record()
                    .failures
                    .iter()
                    .any(|failure| failure.contains("lost app removal reply"))
            );
        }
    }
}

#[test]
fn pending_known_create_is_reconciled_without_a_resource_summary() {
    let (runtime, docker, container) = failed_create_fixture();
    let instance = name("alpha");
    let previous = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
        .unwrap()
        .unwrap();
    let dependencies = previous
        .record()
        .mutations
        .last()
        .unwrap()
        .dependencies
        .clone();
    drop(previous);
    let mut journal = RecoveryJournal::create(runtime.recovery_path(&instance), &instance).unwrap();
    journal
        .begin(container, dependencies, MutationOperation::Create)
        .unwrap();
    assert!(journal.record().resources.is_empty());
    drop(journal);
    let report = runtime
        .cleanup_with(&instance, &docker, false, &cleanup_policy())
        .unwrap();
    assert!(report.verified(), "{report:?}");
    assert!(docker.resources.lock().unwrap().is_empty());
    assert!(!runtime.instance_dir(&instance).exists());
}

fn assert_container_removed_before_dependencies(docker: &StatefulDocker) {
    let calls = docker.calls();
    let removal = calls
        .iter()
        .position(|call| call == &["rm", "-f", "app-id"])
        .unwrap();
    let listing = calls
        .iter()
        .enumerate()
        .find(|(index, call)| {
            *index > removal
                && *call == &["container", "ls", "-a", "--no-trunc", "--format", "{{.ID}}"]
        })
        .unwrap()
        .0;
    for target in ["ths-alpha-wallet", "net-id"] {
        let dependency = calls
            .iter()
            .position(|call| call.len() == 3 && call[1] == "rm" && call[2] == target)
            .unwrap();
        assert!(dependency > listing, "{calls:?}");
    }
}

fn assert_allocation_dependencies_kept(runtime: &Runtime, docker: &StatefulDocker) {
    assert!(docker.contains_id("net-id"));
    assert!(docker.contains_id("ths-alpha-wallet"));
    assert!(runtime.recovery_path(&name("alpha")).exists());
    assert!(!docker.removal_calls().iter().any(|call| {
        matches!(call.as_slice(), [kind, operation, target] if operation == "rm"
            && ((kind == "volume" && target == "ths-alpha-wallet") || (kind == "network" && target == "net-id")))
    }));
}

#[test]
fn failed_start_recovery_can_be_retried() {
    for partial in [true, false] {
        let (runtime, docker, _) =
            failed_start_fixture(MutationOutcome::Uncertain("start failed".into()));
        let instance = name("alpha");
        let report = runtime
            .cleanup_with(&instance, &docker, partial, &cleanup_policy())
            .unwrap();
        assert!(report.verified(), "{report:?}");
        assert!(docker.resources.lock().unwrap().is_empty());
        assert!(!runtime.instance_dir(&instance).exists());
        let mut journal =
            RecoveryJournal::create(runtime.recovery_path(&instance), &instance).unwrap();
        for (kind, target, id) in [
            ("container", "ths-alpha-app", "fresh-app-id"),
            ("volume", "ths-alpha-wallet", "ths-alpha-wallet"),
            ("network", "ths-alpha", "fresh-net-id"),
        ] {
            docker.insert_owned(kind, target, id, "alpha");
            let index = journal
                .begin(
                    ResourceRef {
                        kind: resource_kind(kind),
                        name: target.into(),
                        identity: Some(id.into()),
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
            .cleanup_with(&instance, &docker, partial, &cleanup_policy())
            .unwrap();
        assert!(report.verified(), "{report:?}");
        assert!(docker.resources.lock().unwrap().is_empty());
        assert!(!runtime.instance_dir(&instance).exists());
    }
}

#[test]
fn pending_known_start_is_reconciled_after_reload() {
    let (runtime, docker, _) = failed_start_fixture(MutationOutcome::Pending);
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, false, &cleanup_policy())
        .unwrap();
    assert!(report.verified(), "{report:?}");
    assert!(docker.resources.lock().unwrap().is_empty());
    assert!(!runtime.instance_dir(&name("alpha")).exists());
    assert_container_removed_before_dependencies(&docker);
}

#[test]
fn missing_failed_start_identity_releases_dependencies() {
    let (runtime, docker, _) = failed_start_fixture(MutationOutcome::Pending);
    docker.drop_resource("app-id");
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, false, &cleanup_policy())
        .unwrap();
    assert!(report.verified(), "{report:?}");
    assert!(docker.resources.lock().unwrap().is_empty());
    assert!(!runtime.instance_dir(&name("alpha")).exists());
    assert!(
        docker
            .calls()
            .iter()
            .any(|call| call == &["container", "ls", "-a", "--no-trunc", "--format", "{{.ID}}"])
    );
    assert!(!docker.removal_calls().iter().any(|call| {
        call.iter()
            .any(|arg| arg == "app-id" || arg == "ths-alpha-app")
    }));
}

#[test]
fn failed_start_keeps_dependencies_until_absence_is_verified() {
    let (runtime, docker, _) = failed_start_fixture(MutationOutcome::Pending);
    docker.acknowledge_without_removing("app-id");
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, true, &cleanup_policy())
        .unwrap();
    assert!(!report.verified(), "{report:?}");
    assert!(docker.contains_id("app-id"));
    assert_allocation_dependencies_kept(&runtime, &docker);
    docker.retain.lock().unwrap().clear();
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, false, &cleanup_policy())
        .unwrap();
    assert!(report.verified(), "{report:?}");
    assert!(docker.resources.lock().unwrap().is_empty());
}

#[test]
fn failed_start_inspection_error_keeps_dependencies() {
    let (runtime, docker, _) = failed_start_fixture(MutationOutcome::Pending);
    docker.fail_inspect("app-id");
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, false, &cleanup_policy())
        .unwrap();
    assert!(!report.verified(), "{report:?}");
    assert!(docker.contains_id("app-id"));
    assert_allocation_dependencies_kept(&runtime, &docker);
    assert!(
        !docker
            .removal_calls()
            .iter()
            .any(|call| call == &["rm", "-f", "app-id"])
    );
}

#[test]
fn unidentified_start_keeps_dependencies() {
    for identity in [None, Some(String::new())] {
        for outcome in [
            MutationOutcome::Pending,
            MutationOutcome::Uncertain("lost reply".into()),
        ] {
            let (runtime, docker, mut container) = failed_start_fixture(MutationOutcome::Pending);
            let instance = name("alpha");
            let previous = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
                .unwrap()
                .unwrap();
            let dependencies = previous
                .record()
                .mutations
                .last()
                .unwrap()
                .dependencies
                .clone();
            drop(previous);
            let mut journal =
                RecoveryJournal::create(runtime.recovery_path(&instance), &instance).unwrap();
            let create = journal
                .begin(
                    container.clone(),
                    dependencies.clone(),
                    MutationOperation::Create,
                )
                .unwrap();
            journal
                .finish(
                    create,
                    MutationOutcome::Acknowledged,
                    container.identity.clone(),
                )
                .unwrap();
            container.identity = identity.clone();
            let index = journal
                .begin(container.clone(), dependencies, MutationOperation::Start)
                .unwrap();
            journal.finish(index, outcome, identity.clone()).unwrap();
            drop(journal);
            let report = runtime
                .cleanup_with(&instance, &docker, true, &cleanup_policy())
                .unwrap();
            assert!(!report.verified(), "{report:?}");
            assert!(docker.contains_id("app-id"));
            assert_allocation_dependencies_kept(&runtime, &docker);
        }
    }
}

#[test]
fn resolved_start_does_not_release_shared_unresolved_create_dependencies() {
    let (runtime, docker, _) = failed_start_fixture(MutationOutcome::Pending);
    let instance = name("alpha");
    let mut journal = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
        .unwrap()
        .unwrap();
    journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Container,
                name: "ths-alpha-init".into(),
                identity: None,
            },
            journal
                .record()
                .mutations
                .last()
                .unwrap()
                .dependencies
                .clone(),
            MutationOperation::Create,
        )
        .unwrap();
    drop(journal);
    let report = runtime
        .cleanup_with(&instance, &docker, true, &cleanup_policy())
        .unwrap();
    assert!(!report.verified(), "{report:?}");
    assert!(!docker.contains_id("app-id"));
    assert!(docker.contains_id("init-id"));
    assert_allocation_dependencies_kept(&runtime, &docker);
    let journal = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
        .unwrap()
        .unwrap();
    assert!(
        journal
            .record()
            .mutations
            .iter()
            .any(|mutation| mutation.operation == MutationOperation::Start
                && mutation.outcome == MutationOutcome::ReconciledAbsent)
    );
}

#[test]
fn failed_start_reconciliation_survives_an_interrupted_cleanup() {
    let (runtime, docker, container) = failed_start_fixture(MutationOutcome::Pending);
    let instance = name("alpha");
    let mut journal = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
        .unwrap()
        .unwrap();
    let index = journal
        .begin(container, Vec::new(), MutationOperation::Remove)
        .unwrap();
    journal
        .finish(
            index,
            MutationOutcome::ReconciledAbsent,
            Some("app-id".into()),
        )
        .unwrap();
    drop(journal);
    docker.drop_resource("app-id");
    let report = runtime
        .cleanup_with(&instance, &docker, false, &cleanup_policy())
        .unwrap();
    assert!(report.verified(), "{report:?}");
    assert!(docker.resources.lock().unwrap().is_empty());
    assert!(!runtime.instance_dir(&instance).exists());
}

#[test]
fn failed_start_never_adopts_a_replacement() {
    let (runtime, docker, _) = failed_start_fixture(MutationOutcome::Pending);
    docker.drop_resource("app-id");
    docker.insert_owned("container", "ths-alpha-app", "replacement-id", "alpha");
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, true, &cleanup_policy())
        .unwrap();
    assert!(!report.verified(), "{report:?}");
    assert!(docker.contains_id("replacement-id"));
    assert_allocation_dependencies_kept(&runtime, &docker);
    assert!(!docker.removal_calls().iter().any(|call| {
        call.iter()
            .any(|arg| arg == "replacement-id" || arg == "ths-alpha-app")
    }));
}

#[test]
fn unresolved_create_does_not_match_a_start() {
    let (runtime, _docker, _) = failed_start_fixture(MutationOutcome::Pending);
    let instance = name("alpha");
    let mut journal = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
        .unwrap()
        .unwrap();
    assert!(
        recovery::unresolved_create(journal.record(), &ResourceKind::Container, "ths-alpha-app")
            .is_none()
    );
    let resource = journal.record().mutations.last().unwrap().resource.clone();
    journal
        .begin(resource, Vec::new(), MutationOperation::Create)
        .unwrap();
    assert!(
        recovery::unresolved_create(journal.record(), &ResourceKind::Container, "ths-alpha-app")
            .is_some()
    );
}

#[test]
fn failed_start_reconciliation_write_failure_keeps_the_valid_record() {
    let (runtime, _docker, container) = failed_start_fixture(MutationOutcome::Pending);
    let instance = name("alpha");
    let path = runtime.recovery_path(&instance);
    let mut journal = RecoveryJournal::load(path.clone(), &instance)
        .unwrap()
        .unwrap();
    let previous = journal.record().clone();
    journal.fail_next_write();
    assert!(journal.reconcile_absent_allocations(&container).is_err());
    assert_eq!(journal.record(), &previous);
    assert_eq!(
        RecoveryJournal::load(path, &instance)
            .unwrap()
            .unwrap()
            .record(),
        &previous
    );
    journal.reconcile_absent_allocations(&container).unwrap();
    assert_eq!(
        journal.record().mutations.last().unwrap().outcome,
        MutationOutcome::ReconciledAbsent
    );
    assert_eq!(
        journal.record().mutations[2].outcome,
        MutationOutcome::Acknowledged
    );
    let mut invalid = container;
    invalid.identity = None;
    assert!(journal.reconcile_absent_allocations(&invalid).is_err());
    invalid.identity = Some(String::new());
    assert!(journal.reconcile_absent_allocations(&invalid).is_err());
    invalid.kind = ResourceKind::Volume;
    invalid.identity = Some("app-id".into());
    assert!(journal.reconcile_absent_allocations(&invalid).is_err());
}

#[test]
fn failed_allocation_cleanup_gates_dependencies_on_reconciliation_write() {
    for create in [false, true] {
        struct FailReconciliation<'a> {
            docker: &'a StatefulDocker,
            temporary: std::path::PathBuf,
        }
        impl DockerResourceCommands for FailReconciliation<'_> {
            fn output(&self, args: &[&str]) -> Result<String> {
                let output = self.docker.output(args)?;
                if args == ["container", "ls", "-a", "--format", "{{.Names}}"]
                    && !self.docker.contains_id("app-id")
                    && !self.temporary.exists()
                {
                    fs::create_dir(&self.temporary)?;
                }
                Ok(output)
            }
            fn run(&self, args: &[&str]) -> Result<()> {
                self.docker.run(args)
            }
        }
        let (runtime, docker, _) = if create {
            failed_create_fixture()
        } else {
            failed_start_fixture(MutationOutcome::Pending)
        };
        let instance = name("alpha");
        let temporary = runtime.recovery_path(&instance).with_extension("json.tmp");
        let failed = FailReconciliation {
            docker: &docker,
            temporary: temporary.clone(),
        };
        let report = runtime
            .cleanup_with(&instance, &failed, true, &cleanup_policy())
            .unwrap();
        assert!(!report.verified(), "{report:?}");
        assert!(!docker.contains_id("app-id"));
        assert_allocation_dependencies_kept(&runtime, &docker);
        assert!(
            report.failures.iter().any(
                |failure| failure.contains("reconciling absent") && failure.contains("writing")
            ),
            "{report:?}"
        );
        let journal = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
            .unwrap()
            .unwrap();
        assert!(journal.record().mutations.iter().any(unresolved_allocation));
        drop(journal);
        fs::remove_dir(temporary).unwrap();
        let report = runtime
            .cleanup_with(&instance, &docker, false, &cleanup_policy())
            .unwrap();
        assert!(report.verified(), "{report:?}");
        assert!(docker.resources.lock().unwrap().is_empty());
    }
}

#[test]
fn lost_failed_start_removal_reply_preserves_diagnostics_and_releases_dependencies() {
    struct LostRemoval<'a>(&'a StatefulDocker);
    impl DockerResourceCommands for LostRemoval<'_> {
        fn output(&self, args: &[&str]) -> Result<String> {
            self.0.output(args)
        }
        fn run(&self, args: &[&str]) -> Result<()> {
            if args == ["volume", "rm", "ths-alpha-chain"] {
                anyhow::bail!("independent volume failure");
            }
            self.0.run(args)?;
            if args == ["rm", "-f", "app-id"] {
                anyhow::bail!("lost app removal reply");
            }
            Ok(())
        }
    }
    let (runtime, docker, _) =
        failed_start_fixture(MutationOutcome::Uncertain("start failed".into()));
    let instance = name("alpha");
    let report = runtime
        .cleanup_with(&instance, &LostRemoval(&docker), true, &cleanup_policy())
        .unwrap();
    assert!(!report.verified(), "{report:?}");
    assert!(!docker.contains_id("app-id"));
    assert!(!docker.contains_id("net-id"));
    assert!(!docker.contains_id("ths-alpha-wallet"));
    assert!(docker.contains_id("ths-alpha-chain"));
    let journal = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
        .unwrap()
        .unwrap();
    assert!(!journal.record().mutations.iter().any(unresolved_allocation));
    assert!(
        journal
            .record()
            .failures
            .iter()
            .any(|failure| failure.contains("lost app removal reply"))
    );
    assert!(
        journal
            .record()
            .failures
            .iter()
            .any(|failure| failure.contains("unexpected docker run"))
    );
}

#[test]
fn failed_start_identity_listing_failure_keeps_dependencies() {
    struct FailedListing<'a>(&'a StatefulDocker);
    impl DockerResourceCommands for FailedListing<'_> {
        fn output(&self, args: &[&str]) -> Result<String> {
            if args == ["container", "ls", "-a", "--no-trunc", "--format", "{{.ID}}"] {
                anyhow::bail!("identity listing failed");
            }
            self.0.output(args)
        }
        fn run(&self, args: &[&str]) -> Result<()> {
            self.0.run(args)
        }
    }
    let (runtime, docker, _) = failed_start_fixture(MutationOutcome::Pending);
    let report = runtime
        .cleanup_with(
            &name("alpha"),
            &FailedListing(&docker),
            true,
            &cleanup_policy(),
        )
        .unwrap();
    assert!(!report.verified(), "{report:?}");
    assert!(!docker.contains_id("app-id"));
    assert_allocation_dependencies_kept(&runtime, &docker);
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, false, &cleanup_policy())
        .unwrap();
    assert!(report.verified(), "{report:?}");
    assert!(docker.resources.lock().unwrap().is_empty());
}

#[test]
fn failed_start_foreign_ownership_and_identity_conflicts_keep_dependencies() {
    for foreign in [true, false] {
        let (runtime, docker, container) = failed_start_fixture(MutationOutcome::Pending);
        let instance = name("alpha");
        if foreign {
            docker.drop_resource("app-id");
            docker.insert_owned("container", "ths-alpha-app", "app-id", "other");
        } else {
            let mut journal = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
                .unwrap()
                .unwrap();
            let mut conflicting = container;
            conflicting.identity = Some("other-id".into());
            journal
                .begin(
                    conflicting,
                    journal
                        .record()
                        .mutations
                        .last()
                        .unwrap()
                        .dependencies
                        .clone(),
                    MutationOperation::Start,
                )
                .unwrap();
        }
        let report = runtime
            .cleanup_with(&instance, &docker, true, &cleanup_policy())
            .unwrap();
        assert!(!report.verified(), "{report:?}");
        assert!(docker.contains_id("app-id"));
        assert_allocation_dependencies_kept(&runtime, &docker);
        assert!(
            !docker
                .removal_calls()
                .iter()
                .any(|call| call == &["rm", "-f", "app-id"])
        );
    }
}

#[test]
fn failed_start_preserves_full_ownership_preflight() {
    let (runtime, docker, _) = failed_start_fixture(MutationOutcome::Pending);
    docker.drop_resource("ths-alpha-chain");
    docker.insert_owned("volume", "ths-alpha-chain", "ths-alpha-chain", "other");
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, false, &cleanup_policy())
        .unwrap();
    assert!(!report.verified(), "{report:?}");
    assert!(docker.contains_id("app-id"));
    assert_allocation_dependencies_kept(&runtime, &docker);
    assert!(docker.removal_calls().is_empty());
}

#[test]
fn pending_start_identity_can_be_loaded_without_a_resource_summary() {
    let (runtime, docker, container) = failed_start_fixture(MutationOutcome::Pending);
    let instance = name("alpha");
    let previous = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
        .unwrap()
        .unwrap();
    let dependencies = previous
        .record()
        .mutations
        .last()
        .unwrap()
        .dependencies
        .clone();
    drop(previous);
    let mut journal = RecoveryJournal::create(runtime.recovery_path(&instance), &instance).unwrap();
    journal
        .begin(container, dependencies, MutationOperation::Start)
        .unwrap();
    assert!(journal.record().resources.is_empty());
    drop(journal);
    let report = runtime
        .cleanup_with(&instance, &docker, false, &cleanup_policy())
        .unwrap();
    assert!(report.verified(), "{report:?}");
    assert!(docker.resources.lock().unwrap().is_empty());
    assert!(!runtime.instance_dir(&instance).exists());
}

fn interrupted_create(
    docker: &StatefulDocker,
    instance: &str,
    kind: ResourceKind,
    target: &str,
    outcome: MutationOutcome,
) -> Runtime {
    let instance = name(instance);
    let runtime = Runtime {
        root: docker.root.path().to_path_buf(),
    };
    fs::create_dir_all(runtime.instance_dir(&instance)).unwrap();
    let mut journal = RecoveryJournal::create(runtime.recovery_path(&instance), &instance).unwrap();
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
    if outcome != MutationOutcome::Pending {
        journal.finish(index, outcome, None).unwrap();
    }
    runtime
}

fn interrupted_creates() -> Vec<(ResourceKind, &'static str, &'static str, MutationOutcome)> {
    let mut cases = Vec::new();
    for outcome in [
        MutationOutcome::Pending,
        MutationOutcome::Uncertain("docker network create timed out".into()),
    ] {
        cases.push((
            ResourceKind::Network,
            "network",
            "ths-alpha",
            outcome.clone(),
        ));
        cases.push((ResourceKind::Volume, "volume", "ths-alpha-chain", outcome));
    }
    cases
}

fn interrupted_cleanup_policy() -> LifecyclePolicy {
    LifecyclePolicy {
        startup_docker: Duration::from_secs(5),
        ..cleanup_policy()
    }
}

#[test]
fn an_interrupted_create_of_an_absent_resource_is_settled() {
    for (kind, _, target, outcome) in interrupted_creates() {
        let docker = StatefulDocker::new();
        let runtime = interrupted_create(&docker, "alpha", kind, target, outcome);
        let instance = name("alpha");

        let report = runtime
            .cleanup_with(&instance, &docker, false, &interrupted_cleanup_policy())
            .unwrap();

        assert!(report.verified(), "{target}: {report:?}");
        assert!(docker.removal_calls().is_empty());
        assert!(!runtime.instance_dir(&instance).exists());
    }
}

#[test]
fn an_interrupted_create_removes_the_resource_it_left_behind() {
    for (kind, docker_kind, target, outcome) in interrupted_creates() {
        let docker = StatefulDocker::new();
        let runtime = interrupted_create(&docker, "alpha", kind, target, outcome);
        let id = if docker_kind == "volume" {
            target
        } else {
            "net-id"
        };
        docker.insert_owned(docker_kind, target, id, "alpha");
        let instance = name("alpha");

        let report = runtime
            .cleanup_with(&instance, &docker, false, &interrupted_cleanup_policy())
            .unwrap();

        assert!(report.verified(), "{target}: {report:?}");
        assert_eq!(docker.removal_calls(), [vec![docker_kind, "rm", id]]);
        assert!(!docker.contains_id(id));
        assert!(!runtime.instance_dir(&instance).exists());
    }
}

#[test]
fn an_interrupted_create_never_removes_a_foreign_resource() {
    for (kind, docker_kind, target, outcome) in interrupted_creates() {
        let docker = StatefulDocker::new();
        let runtime = interrupted_create(&docker, "alpha", kind, target, outcome);
        docker.insert_owned(docker_kind, target, "foreign-id", "other");
        let instance = name("alpha");

        let report = runtime
            .cleanup_with(&instance, &docker, false, &interrupted_cleanup_policy())
            .unwrap();

        assert!(!report.verified(), "{target}: {report:?}");
        assert!(docker.removal_calls().is_empty());
        assert!(docker.contains_id("foreign-id"));
        assert!(runtime.recovery_path(&instance).exists());
        assert!(
            format!("{report:?}").contains("is not owned by ths instance alpha"),
            "{report:?}"
        );
    }
}

fn running_create(
    docker_kind: &str,
    instance: &str,
    target: &str,
    seconds: u32,
) -> (tempfile::TempDir, std::process::Child) {
    let script = tempfile::tempdir().unwrap();
    let path = script.path().join("docker");
    fs::write(&path, format!("#!/bin/sh\nsleep {seconds}\n")).unwrap();
    make_executable(&path);
    let child = std::process::Command::new(&path)
        .args([
            docker_kind,
            "create",
            "--label",
            &format!("com.zakura.ths.instance={instance}"),
            target,
        ])
        .spawn()
        .unwrap();
    (script, child)
}

#[test]
fn an_interrupted_create_is_settled_once_its_running_create_ends() {
    for (kind, docker_kind, target) in [
        (ResourceKind::Network, "network", "ths-awaited"),
        (ResourceKind::Volume, "volume", "ths-awaited-chain"),
    ] {
        let docker = StatefulDocker::new();
        let runtime =
            interrupted_create(&docker, "awaited", kind, target, MutationOutcome::Pending);
        let (_script, mut creating) = running_create(docker_kind, "awaited", target, 1);
        let instance = name("awaited");
        let started = Instant::now();

        let report = runtime
            .cleanup_with(&instance, &docker, false, &interrupted_cleanup_policy())
            .unwrap();

        assert!(report.verified(), "{target}: {report:?}");
        assert!(
            started.elapsed() >= Duration::from_millis(500),
            "{target} settled while its create ran"
        );
        assert!(creating.try_wait().unwrap().is_some());
    }
}

#[test]
fn an_interrupted_create_is_kept_while_its_create_keeps_running() {
    for (kind, docker_kind, target) in [
        (ResourceKind::Network, "network", "ths-running"),
        (ResourceKind::Volume, "volume", "ths-running-chain"),
    ] {
        let docker = StatefulDocker::new();
        let runtime = interrupted_create(
            &docker,
            "running",
            kind.clone(),
            target,
            MutationOutcome::Pending,
        );
        let (_script, mut creating) = running_create(docker_kind, "running", target, 30);
        let instance = name("running");
        let policy = LifecyclePolicy {
            cleanup: Duration::from_millis(600),
            ..interrupted_cleanup_policy()
        };

        let report = runtime
            .cleanup_with(&instance, &docker, false, &policy)
            .unwrap();

        creating.kill().unwrap();
        creating.wait().unwrap();
        assert!(!report.verified(), "{target}: {report:?}");
        assert!(
            format!("{report:?}").contains("is still running"),
            "{report:?}"
        );
        let journal = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
            .unwrap()
            .unwrap();
        assert!(recovery::unresolved_create(journal.record(), &kind, target).is_some());
    }
}

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

#[test]
fn an_interrupted_create_waits_for_this_launchers_unreaped_helpers() {
    for (kind, docker_kind, target, _) in interrupted_creates() {
        let docker = StatefulDocker::new();
        let runtime = interrupted_create(
            &docker,
            "alpha",
            kind.clone(),
            target,
            MutationOutcome::Uncertain("timed out".into()),
        );
        let instance = name("alpha");
        let policy = interrupted_cleanup_policy();
        let context = LifecycleContext::new(policy);
        let script = sleep_script();
        let never = NeverExits;
        let removing =
            script_docker(&script, &context, Duration::from_millis(150)).with_child_ops(&never);
        assert!(removing.run(&[docker_kind, "rm", target]).is_err());
        assert!(!context.helpers_finished());

        let report = runtime
            .cleanup_core(
                &instance,
                &docker,
                true,
                Deadline::after(policy.cleanup),
                &context,
            )
            .unwrap();

        assert!(!report.verified(), "{target}: {report:?}");
        let journal = RecoveryJournal::load(runtime.recovery_path(&instance), &instance)
            .unwrap()
            .unwrap();
        assert!(recovery::unresolved_create(journal.record(), &kind, target).is_some());
    }
}

#[test]
fn a_create_that_times_out_is_left_running_to_finish() {
    let script = tempfile::tempdir().unwrap();
    let path = script.path().join("docker");
    fs::write(&path, "#!/bin/sh\nsleep 1\ntouch \"$THS_MARKER\"\n").unwrap();
    make_executable(&path);
    let marker = script.path().join("created");
    let context = LifecycleContext::new(short_policy());
    let mut docker = script_docker(&script, &context, Duration::from_millis(200));
    docker
        .env
        .push(("THS_MARKER".into(), marker.to_string_lossy().into_owned()));
    let started = Instant::now();

    let error = docker
        .create(&[
            "network",
            "create",
            "--label",
            &label(&name("timeout")),
            "ths-timeout",
        ])
        .unwrap_err();

    assert!(format!("{error:#}").contains("timed out"), "{error:#}");
    assert!(started.elapsed() < Duration::from_millis(900));
    assert!(
        context
            .finish_helpers(Deadline::after(Duration::from_secs(1)))
            .is_empty()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "the timed-out create was killed");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_running_create_past_the_process_listing_cap_is_still_seen() {
    let (_script, mut creating) = running_create("network", "capped", "ths-capped", 5);
    let context = LifecycleContext::new(LifecyclePolicy {
        machine_output_limit: 64,
        ..interrupted_cleanup_policy()
    });

    let running = create_running(
        &context,
        Deadline::after(Duration::from_secs(5)),
        "network",
        "ths-capped",
        &name("capped"),
    );

    creating.kill().unwrap();
    creating.wait().unwrap();
    assert!(!matches!(running, Ok(false)), "{running:?}");
}

#[test]
fn the_process_listing_ignores_the_terminal_width() {
    let command = process_listing();

    assert!(
        command
            .get_envs()
            .any(|(key, value)| key == "COLUMNS" && value.is_none())
    );
}

#[test]
fn a_create_is_not_started_once_shutdown_is_requested() {
    let script = tempfile::tempdir().unwrap();
    let path = script.path().join("docker");
    fs::write(&path, "#!/bin/sh\ntouch \"$THS_MARKER\"\n").unwrap();
    make_executable(&path);
    let marker = script.path().join("created");
    let context = LifecycleContext::new(short_policy());
    let requested = || true;
    let mut docker = script_docker(&script, &context, Duration::from_secs(1));
    docker.cancellation = Cancellation::Observe(&requested);
    docker
        .env
        .push(("THS_MARKER".into(), marker.to_string_lossy().into_owned()));

    let error = docker
        .create(&[
            "network",
            "create",
            "--label",
            &label(&name("late")),
            "ths-late",
        ])
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("shutdown was requested"),
        "{error:#}"
    );
    thread::sleep(Duration::from_millis(200));
    assert!(!marker.exists());
}
