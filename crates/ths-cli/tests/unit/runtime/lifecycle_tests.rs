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
    lifecycle::{Cancellation, ChildOps},
    test_support::{HttpFixture, HttpMode, short_policy},
};

#[path = "lifecycle_tests/cleanup.rs"]
mod cleanup;
#[path = "lifecycle_tests/images.rs"]
mod images;
#[path = "lifecycle_tests/readiness.rs"]
mod readiness;
#[path = "lifecycle_tests/recovery.rs"]
mod recovery_suite;

fn name(value: &str) -> InstanceName {
    value.parse().unwrap()
}

/// Every caller of this policy asserts on outcome *classification*
/// (verified/incomplete/uncertain, retained dependencies, journal contents),
/// never on deadline or termination-reserve math itself. Real persistence
/// (fsync) and scheduling delays can eat a finite budget under heavy
/// parallel test contention, which has nothing to do with the classification
/// logic under test, so the budget here is large enough that in-memory
/// fixture work never approaches it regardless of machine load. A test that
/// exercises actual deadline or reserve math (e.g.
/// `hanging_removal_leaves_the_termination_reserve_for_helpers`) must use its
/// own real, finite policy instead of this one.
fn cleanup_policy() -> LifecyclePolicy {
    LifecyclePolicy {
        cleanup: Duration::from_secs(3600),
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

struct RunningDocker;

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
        child_ops: None,
    }
}

fn hang_on_removal_script() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("docker");
    let mut file = fs::File::create(&path).unwrap();
    write!(
        file,
        "#!/bin/sh\nif [ \"$1\" = \"rm\" ]; then\n  [ -n \"$THS_REMOVAL_MARKER\" ] && touch \"$THS_REMOVAL_MARKER\"\n  sleep 30\nfi\nif [ \"$1\" = \"volume\" ] && [ \"$2\" = \"rm\" ]; then\n  sleep 30\nfi\nif [ \"$1\" = \"network\" ] && [ \"$2\" = \"rm\" ]; then\n  sleep 30\nfi\nif [ \"$1\" = \"container\" ] && [ \"$2\" = \"inspect\" ]; then\n  printf '%s\\n' '[{{\"Id\":\"app-id\",\"Config\":{{\"Labels\":{{\"com.zakura.ths.instance\":\"alpha\"}}}}}}]'\n  exit 0\nfi\nexit 0\n"
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
