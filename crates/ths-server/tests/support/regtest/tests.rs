use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

#[cfg(target_os = "linux")]
use super::synchronous_command_output_with_timeout;
use super::{
    CleanupResource, CommandExecutor, CommandOutput, DockerInspect, DockerResource, FailureRoute,
    RecoveryFailureReporter, RegtestStack, docker_resource_exists_from_inspect,
    published_loopback_port, remaining_timeout, shutdown_after_test_cancellation,
    synchronous_command_output,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Invocation {
    program: String,
    args: Vec<String>,
    capture_stdout: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum InspectionFailure {
    ExecutorTimeout,
    NonzeroStderr(Vec<u8>),
}

#[derive(Default)]
struct RecordingExecutor {
    calls: Mutex<Vec<Invocation>>,
    fail_node_run: AtomicBool,
    failing_cleanup_name: Mutex<Option<String>>,
    failed_removal_once_name: Mutex<Option<String>>,
    inspection_failures: Mutex<HashMap<String, InspectionFailure>>,
    removal_attempts: Mutex<HashMap<String, usize>>,
    exited_container: AtomicBool,
}

impl RecordingExecutor {
    fn set_failing_cleanup_name(&self, name: String) {
        *self.failing_cleanup_name.lock().unwrap() = Some(name);
    }

    fn set_failed_removal_once_name(&self, name: String) {
        *self.failed_removal_once_name.lock().unwrap() = Some(name);
    }

    fn set_inspection_failure(&self, name: String, failure: InspectionFailure) {
        self.inspection_failures
            .lock()
            .unwrap()
            .insert(name, failure);
    }

    fn calls(&self) -> Vec<Invocation> {
        self.calls.lock().unwrap().clone()
    }

    fn resource_inspect_status(&self, name: &str) -> i32 {
        let attempts = self
            .removal_attempts
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or_default();
        let fails_once =
            self.failing_cleanup_name.lock().unwrap().as_deref() == Some(name) && attempts == 1;
        let failed_removal_keeps_resource =
            self.failed_removal_once_name.lock().unwrap().as_deref() == Some(name) && attempts == 1;
        if fails_once || failed_removal_keeps_resource {
            0
        } else {
            1
        }
    }

    fn exited_inspection() -> Vec<u8> {
        serde_json::to_vec(&json!([{
            "State": {"Running": false},
            "NetworkSettings": {"Ports": {}},
        }]))
        .expect("fixed Docker inspection JSON serializes")
    }
}

#[async_trait]
impl CommandExecutor for RecordingExecutor {
    async fn execute(
        &self,
        program: &str,
        args: &[String],
        _timeout: Duration,
        capture_stdout: bool,
    ) -> Result<CommandOutput> {
        self.calls.lock().unwrap().push(Invocation {
            program: program.to_owned(),
            args: args.to_vec(),
            capture_stdout,
        });

        if program != "docker" {
            return Ok(CommandOutput {
                exit_code: Some(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }
        if args.first().is_some_and(|argument| argument == "run")
            && args
                .iter()
                .any(|argument| argument == "zakuracore/zakura:1.6.0")
            && self.fail_node_run.load(Ordering::SeqCst)
        {
            return Ok(CommandOutput {
                exit_code: Some(17),
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }
        if args.first().is_some_and(|argument| argument == "inspect")
            && self.exited_container.load(Ordering::SeqCst)
        {
            return Ok(CommandOutput {
                exit_code: Some(0),
                stdout: Self::exited_inspection(),
                stderr: Vec::new(),
            });
        }
        if args.get(1).is_some_and(|argument| argument == "rm") {
            let exit_code = if let Some(name) = args.last() {
                let mut attempts = self.removal_attempts.lock().unwrap();
                let attempt = attempts.entry(name.clone()).or_default();
                *attempt += 1;
                if self.failed_removal_once_name.lock().unwrap().as_deref() == Some(name)
                    && *attempt == 1
                {
                    Some(23)
                } else {
                    Some(0)
                }
            } else {
                Some(0)
            };
            return Ok(CommandOutput {
                exit_code,
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }
        if args.get(1).is_some_and(|argument| argument == "inspect") {
            let name = args
                .last()
                .expect("fixture Docker inspection always has an exact owned name");
            if let Some(failure) = self.inspection_failures.lock().unwrap().get(name).cloned() {
                return match failure {
                    InspectionFailure::ExecutorTimeout => {
                        Err(anyhow::anyhow!("fixture command timed out"))
                    }
                    InspectionFailure::NonzeroStderr(stderr) => Ok(CommandOutput {
                        exit_code: Some(70),
                        stdout: Vec::new(),
                        stderr,
                    }),
                };
            }
            let status = self.resource_inspect_status(name);
            let stderr = if status == 1 {
                match args.first().map(String::as_str) {
                    Some("network") => format!("Error: No such network: {name}\n").into_bytes(),
                    Some("volume") => {
                        format!("Error response from daemon: get {name}: no such volume\n")
                            .into_bytes()
                    }
                    Some("container") => format!("Error: No such container: {name}\n").into_bytes(),
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            };
            return Ok(CommandOutput {
                exit_code: Some(status),
                stdout: Vec::new(),
                stderr,
            });
        }
        Ok(CommandOutput {
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }
}

fn test_server_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_ths-server"))
}

#[test]
fn regtest_stack_is_sync_for_the_spawned_live_scenario() {
    fn assert_sync<T: Sync>() {}
    assert_sync::<RegtestStack>();
}

#[test]
fn published_loopback_port_requires_one_valid_loopback_binding() -> Result<()> {
    let valid: DockerInspect = serde_json::from_value(json!({
        "State": {"Running": true},
        "NetworkSettings": {
            "Ports": {
                "18232/tcp": [
                    {"HostIp": "0.0.0.0", "HostPort": "19132"},
                    {"HostIp": "127.0.0.1", "HostPort": "19133"},
                ],
            },
        },
    }))?;
    assert_eq!(published_loopback_port(&valid, "18232/tcp")?, 19133);

    let missing: DockerInspect = serde_json::from_value(json!({
        "State": {"Running": true},
        "NetworkSettings": {"Ports": {}},
    }))?;
    assert!(published_loopback_port(&missing, "18232/tcp").is_err());

    let non_loopback: DockerInspect = serde_json::from_value(json!({
        "State": {"Running": true},
        "NetworkSettings": {
            "Ports": {"18232/tcp": [{"HostIp": "0.0.0.0", "HostPort": "19132"}]},
        },
    }))?;
    assert!(published_loopback_port(&non_loopback, "18232/tcp").is_err());

    let malformed: DockerInspect = serde_json::from_value(json!({
        "State": {"Running": true},
        "NetworkSettings": {
            "Ports": {"18232/tcp": [{"HostIp": "127.0.0.1", "HostPort": "not-a-port"}]},
        },
    }))?;
    assert!(published_loopback_port(&malformed, "18232/tcp").is_err());
    Ok(())
}

#[test]
fn rootless_docker_not_found_output_is_absent() -> Result<()> {
    let cases = [
        (
            DockerResource::Network("test-network".to_owned()),
            b"Error response from daemon: network test-network not found\n".as_slice(),
        ),
        (
            DockerResource::Volume("test-volume".to_owned()),
            b"Error response from daemon: get test-volume: no such volume\n".as_slice(),
        ),
        (
            DockerResource::Container("test-container".to_owned()),
            b"Error response from daemon: No such container: test-container\n".as_slice(),
        ),
    ];

    for (resource, stderr) in cases {
        let output = CommandOutput {
            exit_code: Some(1),
            stdout: b"[]\n".to_vec(),
            stderr: stderr.to_vec(),
        };
        assert!(
            !docker_resource_exists_from_inspect(&resource, &output)?,
            "rootless Docker must classify a missing {} as absent",
            resource.kind(),
        );
    }
    Ok(())
}

#[test]
fn synchronous_command_output_retains_child_stdout() {
    let output = synchronous_command_output("printf", &["[]\n".to_owned()])
        .expect("local printf command must complete");

    assert_eq!(output.stdout, b"[]\n");
}

#[cfg(target_os = "linux")]
#[test]
fn synchronous_command_output_does_not_wait_for_descendant_held_pipes() {
    let timeout = Duration::from_millis(150);
    let started = Instant::now();
    let output = synchronous_command_output_with_timeout(
        "sh",
        &["-c".to_owned(), "sleep 1 & exit 0".to_owned()],
        timeout,
    );

    assert!(output.is_none(), "the descendant keeps both pipes open");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "stream collection must honor the injected deadline"
    );
}

#[tokio::test]
async fn failed_partial_startup_cleans_exact_names_in_reverse_order_and_retries() -> Result<()> {
    let executor = Arc::new(RecordingExecutor::default());
    executor.fail_node_run.store(true, Ordering::SeqCst);
    let fixture_executor: Arc<dyn CommandExecutor> = executor.clone();
    let mut fixture = RegtestStack::with_executor(test_server_path(), fixture_executor)?;
    let expected_names = vec![
        fixture.names.node_container.clone(),
        fixture.names.lightwalletd_volume.clone(),
        fixture.names.chain_volume.clone(),
        fixture.names.network.clone(),
    ];
    executor.set_failing_cleanup_name(fixture.names.lightwalletd_volume.clone());

    assert!(fixture.start().await.is_err());
    let node_run = executor
        .calls()
        .into_iter()
        .find(|call| {
            call.program == "docker"
                && call.args.first().is_some_and(|argument| argument == "run")
                && call
                    .args
                    .windows(2)
                    .any(|arguments| arguments[0] == "--name" && arguments[1] == expected_names[0])
        })
        .expect("fixture must invoke the exact owned node container");
    for pair in [
        ["--volume", &format!("{}:/data", fixture.names.chain_volume)],
        ["--env", "ZAKURA_STATE__CACHE_DIR=/data"],
    ] {
        assert!(node_run.args.windows(2).any(|arguments| arguments == pair));
    }
    assert!(
        node_run
            .args
            .windows(2)
            .any(|arguments| arguments[0] == "--env" && arguments[1] == "UID=0")
    );
    assert!(
        node_run
            .args
            .windows(2)
            .any(|arguments| arguments[0] == "--env" && arguments[1] == "GID=0")
    );
    assert!(fixture.shutdown().await.is_err());
    let first_removals = executor
        .calls()
        .into_iter()
        .filter(|call| {
            call.program == "docker" && call.args.get(1).is_some_and(|argument| argument == "rm")
        })
        .filter_map(|call| call.args.last().cloned())
        .collect::<Vec<_>>();
    assert_eq!(first_removals, expected_names);
    assert!(
        first_removals
            .iter()
            .all(|name| name.starts_with("ths-recovery-"))
    );
    assert_eq!(fixture.cleanup.len(), 1);

    fixture.shutdown().await?;
    assert!(fixture.cleanup.is_empty());
    assert!(fixture.cleanup_errors().is_empty());
    let removals = executor
        .calls()
        .into_iter()
        .filter(|call| {
            call.program == "docker" && call.args.get(1).is_some_and(|argument| argument == "rm")
        })
        .filter_map(|call| call.args.last().cloned())
        .collect::<Vec<_>>();
    assert_eq!(
        removals,
        [
            expected_names[0].clone(),
            expected_names[1].clone(),
            expected_names[2].clone(),
            expected_names[3].clone(),
            expected_names[1].clone(),
        ]
    );
    Ok(())
}

async fn assert_ambiguous_inspection_retains_cleanup_ownership(
    resource_kind: &str,
    failure: InspectionFailure,
) -> Result<()> {
    let executor = Arc::new(RecordingExecutor::default());
    let fixture_executor: Arc<dyn CommandExecutor> = executor.clone();
    let mut fixture = RegtestStack::with_executor(test_server_path(), fixture_executor)?;
    let resource = match resource_kind {
        "network" => DockerResource::Network(fixture.names.network.clone()),
        "volume" => DockerResource::Volume(fixture.names.chain_volume.clone()),
        "container" => DockerResource::Container(fixture.names.node_container.clone()),
        _ => panic!("test must use a supported Docker resource kind"),
    };
    fixture
        .cleanup
        .push(CleanupResource::Docker(resource.clone()));
    executor.set_inspection_failure(resource.name().to_owned(), failure);

    assert!(
        fixture.docker_resource_exists(&resource).await.is_err(),
        "an ambiguous Docker inspection must not mean resource absence"
    );
    assert_eq!(
        fixture.cleanup,
        vec![
            CleanupResource::PrivateDirectory,
            CleanupResource::Docker(resource),
        ]
    );
    assert!(!fixture.shutdown_complete);
    assert!(executor.calls().iter().all(|call| {
        call.program != "docker" || !call.args.get(1).is_some_and(|argument| argument == "rm")
    }));
    Ok(())
}

#[tokio::test]
async fn exact_not_found_inspection_is_absent_for_each_owned_resource_kind() -> Result<()> {
    let executor = Arc::new(RecordingExecutor::default());
    let fixture_executor: Arc<dyn CommandExecutor> = executor.clone();
    let fixture = RegtestStack::with_executor(test_server_path(), fixture_executor)?;
    let resources = [
        DockerResource::Network(fixture.names.network.clone()),
        DockerResource::Volume(fixture.names.chain_volume.clone()),
        DockerResource::Container(fixture.names.node_container.clone()),
    ];

    for resource in &resources {
        assert!(
            !fixture.docker_resource_exists(resource).await?,
            "exact Docker not-found output must prove {} absence",
            resource.kind()
        );
    }
    assert_eq!(fixture.cleanup, vec![CleanupResource::PrivateDirectory]);
    assert!(executor.calls().iter().all(|call| {
        call.program != "docker" || !call.args.get(1).is_some_and(|argument| argument == "rm")
    }));
    assert!(
        executor
            .calls()
            .iter()
            .filter(|call| {
                call.program == "docker"
                    && call
                        .args
                        .get(1)
                        .is_some_and(|argument| argument == "inspect")
            })
            .all(|call| call.capture_stdout),
        "owned Docker inspections must retain the rootless not-found stdout form"
    );
    Ok(())
}

#[tokio::test]
async fn daemon_unavailable_inspection_retains_cleanup_ownership() -> Result<()> {
    assert_ambiguous_inspection_retains_cleanup_ownership(
        "network",
        InspectionFailure::NonzeroStderr(b"fixture Docker daemon unavailable\n".to_vec()),
    )
    .await
}

#[tokio::test]
async fn permission_denied_inspection_retains_cleanup_ownership() -> Result<()> {
    assert_ambiguous_inspection_retains_cleanup_ownership(
        "volume",
        InspectionFailure::NonzeroStderr(
            b"permission denied while trying to connect to the Docker daemon socket\n".to_vec(),
        ),
    )
    .await
}

#[tokio::test]
async fn executor_timeout_inspection_retains_cleanup_ownership() -> Result<()> {
    assert_ambiguous_inspection_retains_cleanup_ownership(
        "container",
        InspectionFailure::ExecutorTimeout,
    )
    .await
}

#[tokio::test]
async fn other_nonzero_inspection_failure_retains_cleanup_ownership() -> Result<()> {
    assert_ambiguous_inspection_retains_cleanup_ownership(
        "network",
        InspectionFailure::NonzeroStderr(b"Docker CLI configuration failed\n".to_vec()),
    )
    .await
}

#[tokio::test]
async fn failed_docker_removal_is_retained_while_later_cleanup_continues() -> Result<()> {
    let executor = Arc::new(RecordingExecutor::default());
    let fixture_executor: Arc<dyn CommandExecutor> = executor.clone();
    let mut fixture = RegtestStack::with_executor(test_server_path(), fixture_executor)?;
    let network = DockerResource::Network(fixture.names.network.clone());
    let volume = DockerResource::Volume(fixture.names.chain_volume.clone());
    fixture
        .cleanup
        .push(CleanupResource::Docker(network.clone()));
    fixture
        .cleanup
        .push(CleanupResource::Docker(volume.clone()));
    executor.set_failed_removal_once_name(volume.name().to_owned());

    assert!(fixture.shutdown().await.is_err());
    assert_eq!(
        fixture.cleanup,
        vec![CleanupResource::Docker(volume.clone())]
    );
    assert_eq!(fixture.cleanup_errors(), &[volume.label()]);
    let removals = executor
        .calls()
        .into_iter()
        .filter(|call| {
            call.program == "docker" && call.args.get(1).is_some_and(|argument| argument == "rm")
        })
        .filter_map(|call| call.args.last().cloned())
        .collect::<Vec<_>>();
    assert_eq!(
        removals,
        [volume.name().to_owned(), network.name().to_owned()]
    );

    fixture.shutdown().await?;
    assert!(fixture.cleanup.is_empty());
    Ok(())
}

#[tokio::test]
async fn reporter_marks_failed_explicit_cleanup_with_cleanup_route() -> Result<()> {
    let executor = Arc::new(RecordingExecutor::default());
    let fixture_executor: Arc<dyn CommandExecutor> = executor.clone();
    let mut fixture = RegtestStack::with_executor(test_server_path(), fixture_executor)?;
    let volume = DockerResource::Volume(fixture.names.chain_volume.clone());
    fixture
        .cleanup
        .push(CleanupResource::Docker(volume.clone()));
    executor.set_failed_removal_once_name(volume.name().to_owned());

    assert!(fixture.shutdown().await.is_err());
    assert!(!fixture.shutdown_complete);
    assert_eq!(fixture.cleanup_errors(), &[volume.label()]);

    let summary = RecoveryFailureReporter::new().summary(Some(&fixture), FailureRoute::Cleanup);
    assert_eq!(summary["cleanup"], "failed");
    assert_eq!(summary["failure_route"], "cleanup");
    Ok(())
}

#[tokio::test]
async fn local_cancellation_runs_explicit_reverse_order_cleanup() -> Result<()> {
    let executor = Arc::new(RecordingExecutor::default());
    let fixture_executor: Arc<dyn CommandExecutor> = executor.clone();
    let mut fixture = RegtestStack::with_executor(test_server_path(), fixture_executor)?;
    let network = DockerResource::Network(fixture.names.network.clone());
    let volume = DockerResource::Volume(fixture.names.chain_volume.clone());
    fixture
        .cleanup
        .push(CleanupResource::Docker(network.clone()));
    fixture
        .cleanup
        .push(CleanupResource::Docker(volume.clone()));
    let (sender, mut cancellation) = tokio::sync::oneshot::channel();
    sender
        .send(())
        .expect("the test-local cancellation receiver is still owned by cleanup");

    shutdown_after_test_cancellation(&mut fixture, &mut cancellation).await?;
    assert!(fixture.cleanup.is_empty());
    let removals = executor
        .calls()
        .into_iter()
        .filter(|call| {
            call.program == "docker" && call.args.get(1).is_some_and(|argument| argument == "rm")
        })
        .filter_map(|call| call.args.last().cloned())
        .collect::<Vec<_>>();
    assert_eq!(
        removals,
        [volume.name().to_owned(), network.name().to_owned()]
    );
    Ok(())
}

#[tokio::test]
async fn exited_services_abort_readiness_and_recovery_without_raw_process_output() -> Result<()> {
    let executor = Arc::new(RecordingExecutor::default());
    executor.exited_container.store(true, Ordering::SeqCst);
    let mut fixture = RegtestStack::with_executor(test_server_path(), executor)?;

    let error = fixture
        .wait_for_node_ready()
        .await
        .expect_err("an exited node must abort readiness");
    assert!(error.to_string().contains("not running"));
    let lightwalletd = fixture.names.lightwalletd_container.clone();
    let error = fixture
        .assert_container_running_with_timeout(&lightwalletd, Duration::from_secs(1))
        .await
        .expect_err("an exited lightwalletd must abort liveness");
    assert!(error.to_string().contains("not running"));

    fixture.api_url = "http://127.0.0.1:19191".to_owned();
    fixture.server_exit_code = Some(71);
    let error = fixture
        .recovery_read::<Value>(RegtestStack::recovery_deadline(), "/api/v1/status")
        .await
        .expect_err("an exited server must abort recovery reads");
    assert!(error.to_string().contains("127.0.0.1:19191"));
    assert!(
        !error
            .to_string()
            .contains("UNIQUE_RAW_PROCESS_OUTPUT_MARKER")
    );
    fixture.shutdown().await
}

#[test]
fn deadline_exhaustion_names_the_fixture_phase() {
    let error = remaining_timeout(
        Instant::now() - Duration::from_millis(1),
        Duration::from_secs(1),
    )
    .expect_err("expired fixture deadline must fail");
    assert!(
        error
            .to_string()
            .contains("fixture phase exceeded its deadline")
    );
}

#[test]
fn reporter_is_available_before_setup_and_emits_only_allowlisted_fields() {
    let mut reporter = RecoveryFailureReporter::new();
    reporter.record_activity(
        "UNIQUE_TEST_SECRET_MARKER_MUST_NOT_ESCAPE",
        "unknown-status",
        "UNIQUE_TEST_SECRET_MARKER_MUST_NOT_ESCAPE",
    );
    let summary = reporter.summary(None, FailureRoute::Setup);
    let object = summary
        .as_object()
        .expect("reporter summary is always a JSON object");
    let expected = [
        "activity",
        "cleanup",
        "exit_codes",
        "failure_route",
        "heights",
        "owned_resources",
        "phase",
        "proxy_counts",
        "txid",
    ];
    assert_eq!(object.len(), expected.len());
    assert!(expected.iter().all(|key| object.contains_key(*key)));
    assert_eq!(summary["phase"], "setup");
    assert_eq!(summary["failure_route"], "setup");
    assert!(
        !summary
            .to_string()
            .contains("UNIQUE_TEST_SECRET_MARKER_MUST_NOT_ESCAPE")
    );
}
