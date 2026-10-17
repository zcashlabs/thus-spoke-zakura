use super::*;

#[test]
fn foreign_volume_collision_preserves_all_resources() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let mut docker = RecordingDocker::new("ths-alpha-app", "ths-alpha-wallet", "");
    docker.inspect(
        "container",
        "ths-alpha-app",
        r#"[{"Id":"owned-container","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
    );
    docker.inspect(
        "volume",
        "ths-alpha-wallet",
        r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"other"}}]"#,
    );

    let error = runtime
        .delete_instance_resources_with(&name("alpha"), &docker)
        .unwrap_err();

    assert!(format!("{error:#}").contains("not owned"), "{error:#}");
    assert!(docker.runs.lock().unwrap().is_empty());
}

#[test]
fn foreign_container_collision_is_not_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let mut docker = RecordingDocker::new("ths-alpha-app", "", "");
    docker.inspect(
        "container",
        "ths-alpha-app",
        r#"[{"Id":"foreign-container","Config":{"Labels":{"com.zakura.ths.instance":"other"}}}]"#,
    );

    let error = runtime
        .delete_instance_resources_with(&name("alpha"), &docker)
        .unwrap_err();

    assert!(format!("{error:#}").contains("not owned"), "{error:#}");
    assert!(docker.runs.lock().unwrap().is_empty());
}

#[test]
fn owned_resources_still_clean_up() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let mut docker = RecordingDocker::new("ths-alpha-app", "ths-alpha-wallet", "ths-alpha");
    docker.inspect(
        "container",
        "ths-alpha-app",
        r#"[{"Id":"owned-container","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
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

    runtime
        .delete_instance_resources_with(&name("alpha"), &docker)
        .unwrap();

    assert_eq!(
        *docker.runs.lock().unwrap(),
        [
            "rm -f owned-container",
            "volume rm ths-alpha-wallet",
            "network rm owned-network",
        ]
    );
}

#[test]
fn unlabeled_network_collision_preserves_data_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let metadata = runtime.instance_dir(&name("alpha"));
    fs::create_dir_all(&metadata).unwrap();
    fs::write(metadata.join("instance.json"), "important data").unwrap();
    let mut docker = RecordingDocker::new("ths-alpha-app", "ths-alpha-wallet", "ths-alpha");
    docker.inspect(
        "container",
        "ths-alpha-app",
        r#"[{"Id":"owned-container","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
    );
    docker.inspect(
        "volume",
        "ths-alpha-wallet",
        r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
    );
    docker.inspect(
        "network",
        "ths-alpha",
        r#"[{"Id":"old-network","Labels":{}}]"#,
    );

    let error = runtime
        .delete_instance_resources_with(&name("alpha"), &docker)
        .unwrap_err();

    assert!(format!("{error:#}").contains("not owned"), "{error:#}");
    assert!(docker.runs.lock().unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(metadata.join("instance.json")).unwrap(),
        "important data"
    );
}

#[test]
fn new_networks_are_labeled_for_owned_cleanup() {
    struct NewNetwork(AtomicBool);

    impl DockerResourceCommands for NewNetwork {
        fn output(&self, args: &[&str]) -> Result<String> {
            match args {
                ["network", "ls", "--format", "{{.Name}}"] => {
                    Ok(if self.0.load(Ordering::SeqCst) {
                        "ths-alpha".into()
                    } else {
                        String::new()
                    })
                }
                ["network", "inspect", "ths-alpha"] => Ok(
                    r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#
                        .into(),
                ),
                _ => bail!("unexpected Docker read: {args:?}"),
            }
        }

        fn run(&self, args: &[&str]) -> Result<()> {
            assert_eq!(
                args,
                [
                    "network",
                    "create",
                    "--label",
                    "com.zakura.ths.instance=alpha",
                    "ths-alpha"
                ]
            );
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    let docker = NewNetwork(AtomicBool::new(false));

    let dir = tempfile::tempdir().unwrap();
    let mut journal =
        recovery::RecoveryJournal::create(dir.path().join(recovery::RECOVERY_FILE), &name("alpha"))
            .unwrap();
    ensure_network_with(
        "ths-alpha",
        &name("alpha"),
        &docker,
        &mut journal,
        Deadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    assert!(docker.0.load(Ordering::SeqCst));
}

#[test]
fn duplicate_network_names_are_rejected() {
    let docker = RecordingDocker::new("", "", "ths-alpha\nths-alpha");

    let dir = tempfile::tempdir().unwrap();
    let mut journal =
        recovery::RecoveryJournal::create(dir.path().join(recovery::RECOVERY_FILE), &name("alpha"))
            .unwrap();
    let error = ensure_network_with(
        "ths-alpha",
        &name("alpha"),
        &docker,
        &mut journal,
        Deadline::after(Duration::from_secs(1)),
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("multiple network resources"),
        "{error:#}"
    );
    assert!(docker.runs.lock().unwrap().is_empty());
}

#[test]
fn volume_create_does_not_accept_a_racing_foreign_volume() {
    struct RacingVolume(AtomicBool);

    impl DockerResourceCommands for RacingVolume {
        fn output(&self, args: &[&str]) -> Result<String> {
            match args {
                ["volume", "ls", "--format", "{{.Name}}"] => Ok(if self.0.load(Ordering::SeqCst) {
                    "ths-alpha-wallet".into()
                } else {
                    String::new()
                }),
                ["volume", "inspect", "ths-alpha-wallet"] => Ok(
                    r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"other"}}]"#
                        .into(),
                ),
                _ => bail!("unexpected Docker read: {args:?}"),
            }
        }

        fn run(&self, args: &[&str]) -> Result<()> {
            assert_eq!(
                args,
                [
                    "volume",
                    "create",
                    "--label",
                    "com.zakura.ths.instance=alpha",
                    "ths-alpha-wallet"
                ]
            );
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let mut journal =
        recovery::RecoveryJournal::create(dir.path().join(recovery::RECOVERY_FILE), &name("alpha"))
            .unwrap();
    let error = ensure_volume_with(
        "ths-alpha-wallet",
        &name("alpha"),
        &RacingVolume(AtomicBool::new(false)),
        &mut journal,
        Deadline::after(Duration::from_secs(1)),
    )
    .unwrap_err();

    assert!(error.to_string().contains("not owned"), "{error:#}");
}

#[test]
fn initial_cleanup_failure_does_not_retry_on_drop() {
    let (mut host, events) = RecordingHost::new();
    host.initial_delete_error = true;
    let (_sender, shutdown) = Shutdown::channel();

    let error = runtime_for_tests()
        .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
        .unwrap_err();

    assert!(error.to_string().contains("resource collision"));
    assert_eq!(*events.lock().unwrap(), ["delete:alpha"]);
}

#[test]
fn readiness_failure_deletes_the_started_instance() {
    let (mut host, events) = RecordingHost::new();
    host.wait_ready_result = Err("dashboard did not become healthy".into());
    let (_sender, shutdown) = Shutdown::channel();
    let err = runtime_for_tests()
        .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
        .unwrap_err();
    assert!(err.to_string().contains("dashboard did not become healthy"));
    let events = events.lock().unwrap().clone();
    let allocate_pos = events
        .iter()
        .position(|e| e == "allocate:alpha")
        .expect("allocate:alpha");
    assert!(
        events[allocate_pos + 1..]
            .iter()
            .any(|e| e == "delete:alpha"),
        "expected delete:alpha after allocate:alpha, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.starts_with("delete:") && !e.ends_with("alpha"))
    );
    assert!(!events.iter().any(|e| e.starts_with("wait_for_shutdown")));
}

fn allocate_replaced_app(docker: &ReplacedApp, mining: MiningConfig) -> Result<WatchedContainer> {
    let dir = tempfile::tempdir()?;
    let mut journal = recovery::RecoveryJournal::create(
        dir.path().join("lifecycle-recovery.json"),
        &name("alpha"),
    )?;
    let volume = journal.begin(
        recovery::ResourceRef {
            kind: recovery::ResourceKind::Volume,
            name: "ths-alpha-wallet".into(),
            identity: Some("ths-alpha-wallet".into()),
        },
        Vec::new(),
        recovery::MutationOperation::Create,
    )?;
    journal.finish(
        volume,
        recovery::MutationOutcome::Acknowledged,
        Some("ths-alpha-wallet".into()),
    )?;
    ensure_app(
        "ths-alpha",
        &name("alpha"),
        &host_ports(0)?,
        docker,
        &mut journal,
        Deadline::after(Duration::from_secs(5)),
        Vec::new(),
        mining,
    )
}

struct ReplacedApp {
    reuse: bool,
    inspect_error: bool,
}

impl DockerResourceCommands for ReplacedApp {
    fn output(&self, args: &[&str]) -> Result<String> {
        match args {
            ["container", "ls", "-a", "--format", "{{.Names}}"] => {
                Ok(if self.reuse { "ths-alpha-app" } else { "" }.into())
            }
            ["container", "inspect", "ths-alpha-app" | "app-1"] => Ok(
                r#"[{"Id":"app-1","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#
                    .into(),
            ),
            ["create", ..] => Ok("app-1".into()),
            ["container", "inspect", "--format", "{{.Created}}", "app-1"] => {
                if self.inspect_error {
                    bail!("Docker is unreachable");
                }
                Ok("2026-01-01T00:00:00Z".into())
            }
            // After creation (or ownership inspection), the name points to a replacement.
            [
                "container",
                "inspect",
                "--format",
                "{{.Created}}",
                "ths-alpha-app",
            ] => Ok("2026-01-02T00:00:00Z".into()),
            _ => bail!("unexpected Docker read: {}", args.join(" ")),
        }
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        bail!("unexpected Docker mutation: {}", args.join(" "))
    }
}

#[test]
fn app_allocation_captures_identity_before_a_name_replacement() {
    for reuse in [false, true] {
        let docker = ReplacedApp {
            reuse,
            inspect_error: false,
        };
        let app = allocate_replaced_app(&docker, MiningConfig::default()).unwrap();
        assert_eq!(app, watched("app-1"));
    }
}

struct CapturingApp {
    created: Mutex<Vec<String>>,
}

impl DockerResourceCommands for CapturingApp {
    fn output(&self, args: &[&str]) -> Result<String> {
        match args {
            ["container", "ls", "-a", "--format", "{{.Names}}"] => Ok(String::new()),
            ["create", ..] => {
                self.created
                    .lock()
                    .unwrap()
                    .extend(args.iter().map(|arg| (*arg).to_owned()));
                Ok("app-1".into())
            }
            ["container", "inspect", "--format", "{{.Created}}", "app-1"] => {
                Ok("2026-01-01T00:00:00Z".into())
            }
            ["container", "inspect", "app-1"] => Ok(
                r#"[{"Id":"app-1","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#
                    .into(),
            ),
            [
                "container",
                "inspect",
                "ths-alpha-app" | "app-1",
                "--format",
                "{{.Id}}",
            ] => Ok("app-1".into()),
            _ => bail!("unexpected Docker read: {}", args.join(" ")),
        }
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        bail!("unexpected Docker mutation: {}", args.join(" "))
    }
}

fn app_env_with(mining: MiningConfig) -> Vec<String> {
    let docker = CapturingApp {
        created: Mutex::new(Vec::new()),
    };
    let dir = tempfile::tempdir().unwrap();
    let mut journal = recovery::RecoveryJournal::create(
        dir.path().join("lifecycle-recovery.json"),
        &name("alpha"),
    )
    .unwrap();
    let volume = journal
        .begin(
            recovery::ResourceRef {
                kind: recovery::ResourceKind::Volume,
                name: "ths-alpha-wallet".into(),
                identity: Some("ths-alpha-wallet".into()),
            },
            Vec::new(),
            recovery::MutationOperation::Create,
        )
        .unwrap();
    journal
        .finish(
            volume,
            recovery::MutationOutcome::Acknowledged,
            Some("ths-alpha-wallet".into()),
        )
        .unwrap();
    ensure_app(
        "ths-alpha",
        &name("alpha"),
        &host_ports(0).unwrap(),
        &docker,
        &mut journal,
        Deadline::after(Duration::from_secs(5)),
        Vec::new(),
        mining,
    )
    .unwrap();
    docker.created.lock().unwrap().clone()
}

#[test]
fn app_container_receives_the_selected_mining_mode() {
    let default = app_env_with(MiningConfig::default());
    assert!(default.contains(&"THS_MINING_MODE=auto".to_owned()));
    assert!(!default.iter().any(|arg| arg.starts_with("THS_BLOCK_TIME=")));

    let interval = app_env_with(MiningConfig {
        mode: MiningMode::Interval,
        block_time: Some(7),
    });
    assert!(interval.contains(&"THS_MINING_MODE=interval".to_owned()));
    assert!(interval.contains(&"THS_BLOCK_TIME=7".to_owned()));

    let manual = app_env_with(MiningConfig {
        mode: MiningMode::Manual,
        block_time: None,
    });
    assert!(manual.contains(&"THS_MINING_MODE=manual".to_owned()));
    assert!(!manual.iter().any(|arg| arg.starts_with("THS_BLOCK_TIME=")));
}

#[test]
fn app_identity_capture_does_not_fall_back_to_a_replacement_on_docker_errors() {
    let docker = ReplacedApp {
        reuse: false,
        inspect_error: true,
    };
    let error = allocate_replaced_app(&docker, MiningConfig::default()).unwrap_err();
    assert!(error.to_string().contains("Docker is unreachable"));
}

#[test]
fn docker_errors_are_not_reported_as_missing_resources() {
    with_unreachable_docker(|| {
        assert!(container_running("ths-alpha-app").is_err());
        assert!(owned_resource(&DockerCli, "volume", "ths-alpha-chain", &name("alpha")).is_err());
        assert!(owned_resource(&DockerCli, "network", "ths-alpha", &name("alpha")).is_err());
        assert!(DockerEvents::default().exists("0123abcd").is_err());
    });
}

#[test]
fn deleting_keeps_metadata_while_docker_is_unreachable() {
    let runtime = Runtime {
        root: std::env::temp_dir().join(format!("ths-unreachable-{}", std::process::id())),
    };
    let name = name("alpha");
    fs::create_dir_all(runtime.instance_dir(&name)).unwrap();
    runtime
        .write_instance(&name, &endpoints_for(&host_ports(0).unwrap()))
        .unwrap();
    let err = with_unreachable_docker(|| runtime.stop(&name)).unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("container ths-alpha-app"),
        "container missing from {message}"
    );
    assert!(runtime.read_instance(&name).is_ok(), "metadata was deleted");
    fs::remove_dir_all(&runtime.root).unwrap();
}

#[test]
fn commands_report_docker_errors_instead_of_a_stopped_environment() {
    let runtime = runtime_for_tests();
    let name = name("alpha");
    let err = with_unreachable_docker(|| runtime.mine(&name, 1, false)).unwrap_err();
    assert!(
        !err.to_string().contains("is not running"),
        "reported a Docker error as a stopped environment: {err}"
    );
}
