use super::*;

#[test]
fn validates_instance_names() {
    for valid in ["default", "project-2", "a"] {
        assert!(valid.parse::<InstanceName>().is_ok());
    }
    for invalid in ["", "UPPER", "with space", "-start", "end-"] {
        assert!(invalid.parse::<InstanceName>().is_err());
    }
}

#[test]
fn shutdown_reports_interrupt_from_channel() {
    let (sender, shutdown) = Shutdown::channel();
    assert!(!shutdown.try_interrupted());
    sender.send(Ended::Interrupted).unwrap();
    assert!(shutdown.try_interrupted());
    assert!(shutdown.try_interrupted());
    assert_eq!(shutdown.wait().unwrap(), Ended::Interrupted);
}

#[test]
fn default_offset_uses_stable_loopback_ports() {
    let ports = host_ports(0).unwrap();
    assert_eq!(ports.dashboard, 32805);
    assert_eq!(ports.rpc, 18232);
    assert_eq!(ports.p2p, 18233);
    assert_eq!(ports.lightwalletd, 9067);
    assert_eq!(loopback_publish(ports.rpc, 18232), "127.0.0.1:18232:18232");
    assert_eq!(
        loopback_publish(ports.dashboard, 8080),
        "127.0.0.1:32805:8080"
    );
}

#[test]
fn port_offset_shifts_all_four_hosts_by_the_same_stride() {
    let base = host_ports(0).unwrap();
    let shifted = host_ports(10).unwrap();
    assert_eq!(shifted.dashboard, base.dashboard + 10);
    assert_eq!(shifted.rpc, base.rpc + 10);
    assert_eq!(shifted.p2p, base.p2p + 10);
    assert_eq!(shifted.lightwalletd, base.lightwalletd + 10);
}

#[test]
fn port_offset_rejects_values_that_are_not_multiples_of_ten() {
    let err = host_ports(1).unwrap_err();
    assert!(err.to_string().contains('1'));
}

#[test]
fn port_offset_accepts_the_largest_value_that_keeps_every_port_in_range() {
    let ports = host_ports(32730).unwrap();
    assert_eq!(ports.dashboard, 65535);
}

#[test]
fn port_offset_rejects_values_that_overflow_a_host_port() {
    let err = host_ports(32740).unwrap_err();
    assert!(err.to_string().contains("overflow"));
}

#[test]
fn invalid_port_offset_is_rejected_before_any_deletion_or_allocation() {
    let (mut host, events) = RecordingHost::new();
    host.initial_delete_error = true;
    let (_sender, shutdown) = Shutdown::channel();

    let error = runtime_for_tests()
        .start_with(&name("alpha"), false, false, 32740, &host, &shutdown)
        .unwrap_err();

    assert!(error.to_string().contains("overflow"));
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn reports_the_conflicting_loopback_port() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let err = require_free_loopback(port).unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains(&port.to_string()),
        "error should name port {port}, got {message}"
    );
}

#[test]
fn endpoint_exports_match_the_variables_run_passes_to_its_child() {
    let endpoints = endpoints_for(&host_ports(0).unwrap());
    assert_eq!(
        endpoint_exports(&name("alpha"), &endpoints),
        "\
export THS_INSTANCE=alpha
export THS_NETWORK=regtest
export THS_API_URL=http://127.0.0.1:32805
export THS_ZAKURA_RPC_URL=http://127.0.0.1:18232
export THS_LIGHTWALLETD_URL=http://127.0.0.1:9067
export THS_LIGHTWALLETD_TLS=false
export THS_P2P_ADDR=127.0.0.1:18233"
    );

    let mut shifted = endpoints_for(&host_ports(10).unwrap());
    shifted.network = "other".to_owned();
    shifted.tls = true;
    let exports = endpoint_exports(&name("beta"), &shifted);
    for expected in [
        "export THS_INSTANCE=beta",
        "export THS_NETWORK=regtest",
        "export THS_API_URL=http://127.0.0.1:32815",
        "export THS_ZAKURA_RPC_URL=http://127.0.0.1:18242",
        "export THS_LIGHTWALLETD_URL=http://127.0.0.1:9077",
        "export THS_LIGHTWALLETD_TLS=false",
        "export THS_P2P_ADDR=127.0.0.1:18243",
    ] {
        assert!(
            exports.lines().any(|line| line == expected),
            "missing {expected}"
        );
    }
}

#[test]
fn endpoints_json_includes_regtest_and_plaintext_lightwalletd() {
    let json = serde_json::to_value(endpoints_for(&host_ports(0).unwrap())).unwrap();
    assert_eq!(json["dashboard"], "http://127.0.0.1:32805");
    assert_eq!(json["rpc"], "http://127.0.0.1:18232");
    assert_eq!(json["lightwalletd"], "http://127.0.0.1:9067");
    assert_eq!(json["p2p"], "127.0.0.1:18233");
    assert_eq!(json["network"], "regtest");
    assert_eq!(json["tls"], false);
}

#[test]
fn endpoints_json_without_network_fields_still_deserializes() {
    let parsed: Endpoints = serde_json::from_str(
        r#"{"dashboard":"http://127.0.0.1:1","rpc":"http://127.0.0.1:2","lightwalletd":"http://127.0.0.1:3","p2p":"127.0.0.1:4"}"#,
    )
    .unwrap();
    assert_eq!(parsed.network, "regtest");
    assert!(!parsed.tls);
}

fn environment(name: &str, states: [&str; 4]) -> EnvironmentStatus {
    let containers = SERVICES
        .into_iter()
        .zip(states)
        .map(|(service, state)| ContainerStatus {
            service,
            state: state.into(),
            created_at: (state != "missing").then_some(1_000),
            started_at: (state == "running" || state == "exited").then_some(1_060),
        })
        .collect::<Vec<_>>();
    EnvironmentStatus {
        instance: Instance {
            name: name.into(),
            version: 1,
            endpoints: endpoints_for(&HostPorts {
                dashboard: 8080,
                rpc: 18232,
                lightwalletd: 9067,
                p2p: 18233,
            }),
            created_at: Some(1_000),
            session: None,
        },
        state: environment_state(&containers),
        containers,
    }
}

#[test]
fn environments_are_classified_by_their_long_running_services() {
    let state = |states| environment("e", states).state;
    assert_eq!(
        state(["running", "running", "running", "exited"]),
        "running"
    );
    assert_eq!(
        state(["exited", "running", "running", "exited"]),
        "degraded"
    );
    assert_eq!(
        state(["missing", "running", "missing", "missing"]),
        "degraded"
    );
    assert_eq!(state(["exited", "exited", "exited", "exited"]), "stopped");
    assert_eq!(
        state(["missing", "missing", "missing", "missing"]),
        "stopped"
    );
    // init is one-shot: running or not, it does not decide the state.
    assert_eq!(state(["exited", "exited", "exited", "running"]), "stopped");
    // a paused service is still up.
    assert_eq!(state(["paused", "paused", "paused", "exited"]), "paused");
    assert_eq!(state(["paused", "running", "running", "exited"]), "paused");
    assert_eq!(state(["paused", "exited", "running", "exited"]), "degraded");
    // any container under its names that it doesn't own, init included.
    assert_eq!(
        state(["foreign", "foreign", "foreign", "missing"]),
        "conflict"
    );
    assert_eq!(
        state(["running", "running", "running", "foreign"]),
        "conflict"
    );
}

#[test]
fn the_list_groups_active_before_inactive_and_hints_only_for_inactive() {
    let active = environment("default", ["running", "running", "running", "exited"]);
    let degraded = environment("partial", ["exited", "running", "running", "exited"]);
    let stopped = environment("old", ["missing", "missing", "missing", "missing"]);
    let rendered = render_list(&[stopped, active, degraded], 1_000 + 7_200);
    assert_eq!(
        rendered,
        concat!(
            "ACTIVE\n",
            "default          http://127.0.0.1:8080      running    created 2h ago\n",
            "  app           running   created 2h ago    started 1h ago\n",
            "  zakura        running   created 2h ago    started 1h ago\n",
            "  lightwalletd  running   created 2h ago    started 1h ago\n",
            "  init          exited    created 2h ago    started 1h ago\n",
            "partial          http://127.0.0.1:8080      degraded   created 2h ago\n",
            "  app           exited    created 2h ago    started 1h ago\n",
            "  zakura        running   created 2h ago    started 1h ago\n",
            "  lightwalletd  running   created 2h ago    started 1h ago\n",
            "  init          exited    created 2h ago    started 1h ago\n",
            "\n",
            "INACTIVE\n",
            "old              http://127.0.0.1:8080      stopped    created 2h ago\n",
            "  app           missing\n",
            "  zakura        missing\n",
            "  lightwalletd  missing\n",
            "  init          missing\n",
            "\n",
            "Remove an inactive environment and all of its data with:\n",
            "  ths --name <name> stop\n",
        )
    );
    // no inactive environment: no empty section and no hint.
    let only_active = render_list(
        &[environment(
            "default",
            ["running", "running", "running", "exited"],
        )],
        1_000,
    );
    assert!(!only_active.contains("INACTIVE") && !only_active.contains("ths --name"));
    let only_inactive = render_list(
        &[environment("old", ["exited", "exited", "exited", "exited"])],
        1_000,
    );
    assert!(only_inactive.starts_with("INACTIVE\n") && only_inactive.contains("ths --name"));
}

#[test]
fn the_json_list_keeps_its_fields_and_adds_absolute_status() {
    let json = serde_json::to_value(environment(
        "default",
        ["running", "exited", "missing", "exited"],
    ))
    .unwrap();
    assert_eq!(json["name"], "default");
    assert_eq!(json["version"], 1);
    assert_eq!(json["endpoints"]["dashboard"], "http://127.0.0.1:8080");
    assert_eq!(json["state"], "degraded");
    assert_eq!(json["created_at"], 1_000);
    assert_eq!(json["containers"][0]["service"], "app");
    assert_eq!(json["containers"][0]["started_at"], 1_060);
    assert_eq!(json["containers"][2]["state"], "missing");
    assert!(json["containers"][2]["created_at"].is_null());
}

#[test]
fn environments_recorded_before_creation_times_still_list() {
    let mut old = environment("old", ["missing", "missing", "missing", "missing"]);
    let mut legacy = serde_json::to_value(&old.instance).unwrap();
    legacy.as_object_mut().unwrap().remove("created_at");
    old.instance = serde_json::from_value(legacy).unwrap();
    assert_eq!(old.instance.created_at, None);
    assert!(
        render_list(&[old], 1_000)
            .starts_with("INACTIVE\nold              http://127.0.0.1:8080      stopped\n")
    );
}

#[test]
fn environments_still_list_when_docker_is_unavailable() {
    let saved = environment("default", ["running", "running", "running", "exited"]).instance;
    let unknown = environment_status(saved, Err(anyhow!("Cannot connect to the Docker daemon")));
    let json = serde_json::to_value(&unknown).unwrap();
    assert_eq!(json["name"], "default");
    assert_eq!(json["endpoints"]["dashboard"], "http://127.0.0.1:8080");
    assert_eq!(json["created_at"], 1_000);
    assert_eq!(json["state"], "unknown");
    assert_eq!(json["containers"], serde_json::json!([]));
    assert_eq!(
        render_list(&[unknown], 1_000 + 7_200),
        "UNKNOWN\ndefault          http://127.0.0.1:8080      unknown    created 2h ago\n"
    );
}

/// docker holding alpha's service containers, each with an optional owner label and a status.
fn docker_holding(containers: &[(&str, Option<&str>, &str)]) -> RecordingDocker {
    let names: Vec<_> = containers
        .iter()
        .map(|(service, ..)| format!("ths-alpha-{service}"))
        .collect();
    let mut docker = RecordingDocker::new(&names.join("\n"), "", "");
    for (service, owner, status) in containers {
        let labels = owner.map_or("{}".to_owned(), |owner| {
            format!(r#"{{"{INSTANCE_LABEL}":"{owner}"}}"#)
        });
        docker.inspect(
            "container",
            &format!("ths-alpha-{service}"),
            &format!(r#"[{{"Id":"{service}-id","Config":{{"Labels":{labels}}}}}]"#),
        );
        docker.extra_output.insert(
            format!(
                "container inspect --format {{{{.State.Status}}}} {{{{.Created}}}} {{{{.State.StartedAt}}}} {service}-id"
            ),
            format!("{status} 2026-10-09T10:00:00Z 2026-10-09T10:01:00Z"),
        );
    }
    docker
}

#[test]
fn listing_checks_container_ownership_and_reports_paused_stacks() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    fs::create_dir_all(runtime.instance_dir(&name("alpha"))).unwrap();
    runtime
        .write_instance(&name("alpha"), &endpoints_for(&host_ports(0).unwrap()))
        .unwrap();
    let listed = |docker: &RecordingDocker| {
        let environments = runtime.environments(docker).unwrap();
        let containers: Vec<_> = environments[0]
            .containers
            .iter()
            .map(|c| c.state.clone())
            .collect();
        let rendered = render_list(&environments, now_unix().unwrap());
        (environments[0].state, containers, rendered)
    };

    // stale metadata over unrelated, unlabeled containers holding the service names.
    let (state, containers, rendered) = listed(&docker_holding(&[
        ("app", None, "running"),
        ("zakura", None, "running"),
        ("lightwalletd", None, "running"),
    ]));
    assert_eq!(state, "conflict");
    assert_eq!(containers, ["foreign", "foreign", "foreign", "missing"]);
    assert!(
        rendered.starts_with("CONFLICT\nalpha") && !rendered.contains("ths --name"),
        "{rendered}"
    );
    // a container another instance owns is foreign too.
    let (state, ..) = listed(&docker_holding(&[
        ("app", Some("other"), "running"),
        ("zakura", Some("alpha"), "running"),
        ("lightwalletd", Some("alpha"), "running"),
    ]));
    assert_eq!(state, "conflict");

    for (statuses, expected) in [
        (["running", "running", "running"], "running"),
        (["paused", "paused", "paused"], "paused"),
        (["paused", "running", "running"], "paused"),
    ] {
        let (state, containers, rendered) = listed(&docker_holding(&[
            ("app", Some("alpha"), statuses[0]),
            ("zakura", Some("alpha"), statuses[1]),
            ("lightwalletd", Some("alpha"), statuses[2]),
        ]));
        assert_eq!(state, expected);
        assert_eq!(containers[..3], statuses);
        assert!(
            rendered.starts_with("ACTIVE\nalpha") && !rendered.contains("ths --name"),
            "{rendered}"
        );
    }
}

#[test]
fn docker_timestamps_parse_to_unix_seconds() {
    assert_eq!(parse_docker_time("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(
        parse_docker_time("2026-10-02T21:58:53.061174304Z"),
        Some(1_790_978_333)
    );
    assert_eq!(
        parse_docker_time("2024-02-29T12:00:00Z"),
        Some(1_709_208_000)
    );
    // docker's zero time for a container that never started.
    assert_eq!(parse_docker_time("0001-01-01T00:00:00Z"), None);
    assert_eq!(parse_docker_time("not a time"), None);
}

#[test]
fn start_refusal_names_what_holds_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let alpha = name("alpha");
    let _held = runtime.claim_start(&alpha).unwrap();
    let refusal = || match runtime.claim_start(&alpha) {
        Ok(_) => panic!("a held start lock was claimed twice"),
        Err(error) => error.to_string(),
    };
    let session = dir.path().join(".locks/alpha.session");
    let stopping = dir.path().join(".locks/alpha.stopping");

    assert_eq!(
        refusal(),
        "environment alpha is already in use by another command"
    );
    fs::write(&session, "session").unwrap();
    assert!(refusal().contains("already running in another `ths start` or `ths run`"));
    let request = dir.path().join(".locks/alpha.session.stop");
    fs::write(&request, "").unwrap();
    assert!(refusal().contains("is being stopped"));
    fs::remove_file(&request).unwrap();
    fs::remove_file(&session).unwrap();
    fs::write(&stopping, "").unwrap();
    assert!(refusal().contains("is being stopped"));
}

#[test]
fn an_exiting_session_keeps_a_newer_session_file() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let alpha = name("alpha");
    let (_sender, shutdown) = Shutdown::channel();
    let session_file = dir.path().join(".locks/alpha.session");
    fs::create_dir_all(dir.path().join(".locks")).unwrap();

    let old = runtime
        .publish_session(&alpha, &shutdown, Duration::from_millis(5))
        .unwrap();
    fs::write(&session_file, "newer-session").unwrap();
    drop(old);
    assert_eq!(fs::read_to_string(&session_file).unwrap(), "newer-session");

    fs::remove_file(&session_file).unwrap();
    let only = runtime
        .publish_session(&alpha, &shutdown, Duration::from_millis(5))
        .unwrap();
    assert!(session_file.exists());
    drop(only);
    assert!(!session_file.exists());
}

#[test]
fn owner_probes_never_refuse_a_start() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let alpha = name("alpha");
    assert!(!runtime.owner_active(&alpha).unwrap());
    let held = runtime.claim_start(&alpha).unwrap();
    assert!(runtime.owner_active(&alpha).unwrap());
    drop(held);

    let probing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let prober = {
        let runtime = Runtime {
            root: dir.path().to_path_buf(),
        };
        let probing = probing.clone();
        std::thread::spawn(move || {
            let alpha = name("alpha");
            while probing.load(std::sync::atomic::Ordering::SeqCst) {
                runtime.owner_active(&alpha).unwrap();
            }
        })
    };
    for _ in 0..200 {
        drop(runtime.claim_start(&alpha).unwrap());
        drop(runtime.lock_instance(&alpha).unwrap());
    }
    probing.store(false, std::sync::atomic::Ordering::SeqCst);
    prober.join().unwrap();
}

#[test]
fn listed_environments_do_not_expose_the_launcher_session() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let alpha = name("alpha");
    fs::create_dir_all(dir.path().join(".locks")).unwrap();
    fs::write(dir.path().join(".locks/alpha.session"), "secret-session").unwrap();
    fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
    runtime
        .write_instance(&alpha, &endpoints_for(&host_ports(0).unwrap()))
        .unwrap();
    assert_eq!(
        runtime.read_instance(&alpha).unwrap().session.as_deref(),
        Some("secret-session")
    );

    let environments = runtime
        .environments(&docker_holding(&[("app", Some("alpha"), "running")]))
        .unwrap();
    let listed = serde_json::to_string(&environments).unwrap();
    assert!(!listed.contains("session"), "{listed}");
}

#[test]
fn a_damaged_generation_file_does_not_lock_the_name_out() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let alpha = name("alpha");
    fs::create_dir_all(dir.path().join(".locks")).unwrap();
    fs::write(dir.path().join(".locks/alpha.generation"), "").unwrap();

    drop(runtime.claim_start(&alpha).unwrap());

    assert_eq!(
        fs::read_to_string(dir.path().join(".locks/alpha.generation")).unwrap(),
        "1"
    );
}
