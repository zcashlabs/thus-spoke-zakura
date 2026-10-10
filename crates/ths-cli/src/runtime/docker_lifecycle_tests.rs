//! Ignored Docker regressions for lost replies and partial cleanup.
//!
//! These tests are not part of the Docker-free suite. They need a disposable
//! daemon. Lost replies prove a successful daemon effect plus reconciliation;
//! they do not prove that a still-pending daemon request was cancelled.

use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context, Result, bail};

use super::recovery::{
    MutationOperation, MutationOutcome, RecoveryJournal, ResourceKind, ResourceRef,
};
use super::*;
use crate::lifecycle::Cancellation;

struct LiveObservation {
    report: CleanupReport,
    actual_resource_present: bool,
    mutation_calls: usize,
    recovery_present: bool,
    dependencies_present: bool,
    foreign_unchanged: bool,
    other_instance_unchanged: bool,
}

struct LostReplyDocker<'a> {
    inner: LifecycleDocker<'a>,
    mutations: AtomicUsize,
    created: Mutex<Vec<String>>,
}

impl<'a> LostReplyDocker<'a> {
    fn new(context: &'a LifecycleContext, deadline: Deadline, cap: std::time::Duration) -> Self {
        Self {
            inner: LifecycleDocker::production(context, Some(deadline), cap, Cancellation::Ignore),
            mutations: AtomicUsize::new(0),
            created: Mutex::new(Vec::new()),
        }
    }
}

impl DockerResourceCommands for LostReplyDocker<'_> {
    fn output(&self, args: &[&str]) -> Result<String> {
        let text = self.inner.output(args)?;
        if args.first().copied() == Some("create") {
            self.mutations.fetch_add(1, Ordering::SeqCst);
            self.created.lock().expect("created ids").push(text.clone());
            bail!("synthetic timeout after docker {}", args.join(" "));
        }
        Ok(text)
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        let mutation = matches!(
            args,
            ["rm", "-f", _] | ["volume", "rm", _] | ["network", "rm", _]
        );
        if mutation {
            self.inner.run(args)?;
            self.mutations.fetch_add(1, Ordering::SeqCst);
            bail!("synthetic timeout after docker {}", args.join(" "));
        }
        self.inner.run(args)
    }
}

struct SelectiveDocker<'a> {
    inner: LifecycleDocker<'a>,
    fail_volume: String,
}

impl DockerResourceCommands for SelectiveDocker<'_> {
    fn output(&self, args: &[&str]) -> Result<String> {
        self.inner.output(args)
    }

    fn run(&self, args: &[&str]) -> Result<()> {
        if let ["volume", "rm", name] = args
            && *name == self.fail_volume
        {
            bail!("forced removal failure for {name}");
        }
        self.inner.run(args)
    }
}

fn unique_name() -> InstanceName {
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
    format!("lc{suffix}").parse().unwrap()
}

fn exercise_lost_create() -> Result<LiveObservation> {
    let instance = unique_name();
    let prefix = prefix(&instance);
    let root = tempfile::tempdir().context("creating the live runtime root")?;
    let runtime = Runtime {
        root: root.path().to_path_buf(),
    };
    fs::create_dir_all(runtime.instance_dir(&instance))?;
    let network_name = prefix.clone();
    let volume_name = format!("{prefix}-chain");
    let container_name = format!("{prefix}-zakura");
    DockerCli.run(&[
        "network",
        "create",
        "--label",
        &label(&instance),
        &network_name,
    ])?;
    DockerCli.run(&[
        "volume",
        "create",
        "--label",
        &label(&instance),
        &volume_name,
    ])?;
    let network_id =
        DockerCli.output(&["network", "inspect", "--format", "{{.Id}}", &network_name])?;
    let mut journal = RecoveryJournal::create(runtime.recovery_path(&instance), &instance)?;
    let network_ref = ResourceRef {
        kind: ResourceKind::Network,
        name: network_name.clone(),
        identity: Some(network_id.clone()),
    };
    let volume_ref = ResourceRef {
        kind: ResourceKind::Volume,
        name: volume_name.clone(),
        identity: Some(volume_name.clone()),
    };
    remember_acknowledged(&mut journal, network_ref.clone())?;
    remember_acknowledged(&mut journal, volume_ref.clone())?;
    let policy = LifecyclePolicy::default();
    let context = LifecycleContext::new(policy);
    let operation = Deadline::after(policy.startup_docker);
    let lost = LostReplyDocker::new(
        &context,
        Deadline::after(policy.cleanup),
        policy.startup_docker,
    );
    let created = create_container(
        &container_name,
        &instance,
        &lost,
        &mut journal,
        operation,
        vec![network_ref, volume_ref],
        &[
            "create",
            "--name",
            &container_name,
            "--network",
            &network_name,
            "--label",
            &label(&instance),
            ZAKURA_IMAGE,
        ],
    );
    let create_error = created.expect_err("lost create reply");
    assert!(
        create_error.to_string().contains("synthetic timeout"),
        "{create_error:#}"
    );
    drop(journal);
    let report = runtime.cleanup_with(&instance, &lost, true, &policy)?;
    let actual_resource_present = name_present("container", &container_name)?;
    let dependencies_present =
        name_present("network", &network_name)? && name_present("volume", &volume_name)?;
    let recovery_present = runtime.recovery_path(&instance).exists();
    let mutation_calls = lost.mutations.load(Ordering::SeqCst);
    let created_ids = lost.created.lock().expect("created ids").clone();
    for id in &created_ids {
        let _ = DockerCli.run(&["rm", "-f", id]);
    }
    let _ = DockerCli.run(&["network", "rm", &network_id]);
    let _ = DockerCli.run(&["volume", "rm", &volume_name]);
    Ok(LiveObservation {
        report,
        actual_resource_present,
        mutation_calls,
        recovery_present,
        dependencies_present,
        foreign_unchanged: false,
        other_instance_unchanged: false,
    })
}

fn exercise_lost_remove() -> Result<LiveObservation> {
    let instance = unique_name();
    let volume_name = format!("{}-chain", prefix(&instance));
    let root = tempfile::tempdir().context("creating the live runtime root")?;
    let runtime = Runtime {
        root: root.path().to_path_buf(),
    };
    fs::create_dir_all(runtime.instance_dir(&instance))?;
    fs::write(
        runtime.instance_dir(&instance).join("instance.json"),
        b"{}\n",
    )?;
    DockerCli.run(&[
        "volume",
        "create",
        "--label",
        &label(&instance),
        &volume_name,
    ])?;
    let mut journal = RecoveryJournal::create(runtime.recovery_path(&instance), &instance)?;
    remember_acknowledged(
        &mut journal,
        ResourceRef {
            kind: ResourceKind::Volume,
            name: volume_name.clone(),
            identity: Some(volume_name.clone()),
        },
    )?;
    drop(journal);
    let policy = LifecyclePolicy::default();
    let context = LifecycleContext::new(policy);
    let lost = LostReplyDocker::new(
        &context,
        Deadline::after(policy.cleanup),
        policy.startup_docker,
    );
    let report = runtime.cleanup_with(&instance, &lost, false, &policy)?;
    let actual_resource_present = name_present("volume", &volume_name)?;
    let recovery_present = runtime.recovery_path(&instance).exists();
    let mutation_calls = lost.mutations.load(Ordering::SeqCst);
    if actual_resource_present {
        let _ = DockerCli.run(&["volume", "rm", &volume_name]);
    }
    Ok(LiveObservation {
        report,
        actual_resource_present,
        mutation_calls,
        recovery_present,
        dependencies_present: false,
        foreign_unchanged: false,
        other_instance_unchanged: false,
    })
}

fn exercise_partial_cleanup() -> Result<LiveObservation> {
    let owned = unique_name();
    let other = unique_name();
    let owned_prefix = prefix(&owned);
    let other_prefix = prefix(&other);
    let owned_volume = format!("{owned_prefix}-wallet");
    let owned_network = owned_prefix.clone();
    let other_volume = format!("{other_prefix}-wallet");
    let other_network = other_prefix.clone();
    let foreign_volume = format!("foreign-vol-{owned}");
    let foreign_network = format!("foreign-net-{owned}");
    let sentinel = format!("sentinel-{owned}");
    let root = tempfile::tempdir().context("creating the live runtime root")?;
    let runtime = Runtime {
        root: root.path().to_path_buf(),
    };
    fs::create_dir_all(runtime.instance_dir(&owned))?;
    fs::write(runtime.instance_dir(&owned).join("instance.json"), b"{}\n")?;
    fs::create_dir_all(runtime.instance_dir(&other))?;
    fs::write(runtime.instance_dir(&other).join("instance.json"), b"{}\n")?;
    let _ = RecoveryJournal::create(runtime.recovery_path(&owned), &owned)?;
    DockerCli.run(&["volume", "create", "--label", &label(&owned), &owned_volume])?;
    DockerCli.run(&[
        "network",
        "create",
        "--label",
        &label(&owned),
        &owned_network,
    ])?;
    DockerCli.run(&["volume", "create", "--label", &label(&other), &other_volume])?;
    DockerCli.run(&[
        "network",
        "create",
        "--label",
        &label(&other),
        &other_network,
    ])?;
    DockerCli.run(&[
        "volume",
        "create",
        "--label",
        &format!("com.zakura.ths.sentinel={sentinel}"),
        &foreign_volume,
    ])?;
    DockerCli.run(&[
        "network",
        "create",
        "--label",
        &format!("com.zakura.ths.sentinel={sentinel}"),
        &foreign_network,
    ])?;
    let policy = LifecyclePolicy::default();
    let context = LifecycleContext::new(policy);
    let docker = SelectiveDocker {
        inner: LifecycleDocker::production(
            &context,
            Some(Deadline::after(policy.cleanup)),
            policy.startup_docker,
            Cancellation::Ignore,
        ),
        fail_volume: owned_volume.clone(),
    };
    let report = runtime.cleanup_with(&owned, &docker, true, &policy)?;
    let foreign_volume_sentinel = DockerCli
        .output(&[
            "volume",
            "inspect",
            "--format",
            "{{index .Labels \"com.zakura.ths.sentinel\"}}",
            &foreign_volume,
        ])
        .unwrap_or_default();
    let foreign_network_sentinel = DockerCli
        .output(&[
            "network",
            "inspect",
            "--format",
            "{{index .Labels \"com.zakura.ths.sentinel\"}}",
            &foreign_network,
        ])
        .unwrap_or_default();
    let foreign_unchanged = foreign_volume_sentinel.trim() == sentinel
        && foreign_network_sentinel.trim() == sentinel
        && name_present("volume", &foreign_volume)?
        && name_present("network", &foreign_network)?;
    let other_instance_unchanged =
        name_present("volume", &other_volume)? && name_present("network", &other_network)?;
    let recovery_present = runtime.recovery_path(&owned).exists();
    for volume in [&owned_volume, &other_volume, &foreign_volume] {
        let _ = DockerCli.run(&["volume", "rm", volume]);
    }
    for network in [&owned_network, &other_network, &foreign_network] {
        let _ = DockerCli.run(&["network", "rm", network]);
    }
    Ok(LiveObservation {
        report,
        actual_resource_present: name_present("volume", &owned_volume).unwrap_or(true),
        mutation_calls: 0,
        recovery_present,
        dependencies_present: false,
        foreign_unchanged,
        other_instance_unchanged,
    })
}

fn remember_acknowledged(journal: &mut RecoveryJournal, resource: ResourceRef) -> Result<()> {
    let identity = resource.identity.clone();
    let index = journal.begin(resource, Vec::new(), MutationOperation::Create)?;
    journal.finish(index, MutationOutcome::Acknowledged, identity)?;
    Ok(())
}

fn name_present(kind: &str, target: &str) -> Result<bool> {
    let names = match kind {
        "container" => DockerCli.output(&["container", "ls", "-a", "--format", "{{.Names}}"])?,
        "volume" => DockerCli.output(&["volume", "ls", "--format", "{{.Name}}"])?,
        "network" => DockerCli.output(&["network", "ls", "--format", "{{.Name}}"])?,
        _ => bail!("unknown kind {kind}"),
    };
    Ok(names.lines().any(|candidate| candidate.trim() == target))
}

#[test]
#[ignore = "requires a disposable Docker daemon and prepared runtime images"]
fn lost_create_reply_retains_uncertainty_and_recovery() {
    let observed = exercise_lost_create().unwrap();
    assert!(observed.actual_resource_present);
    assert_eq!(observed.mutation_calls, 1);
    assert!(matches!(observed.report.outcome, CleanupOutcome::Uncertain));
    assert!(observed.recovery_present);
    assert!(observed.dependencies_present);
}

#[test]
#[ignore = "requires a disposable Docker daemon"]
fn lost_remove_reply_is_verified_against_daemon_state() {
    let observed = exercise_lost_remove().unwrap();
    assert!(!observed.actual_resource_present);
    assert_eq!(observed.mutation_calls, 1);
    assert!(observed.report.verified());
    assert!(!observed.recovery_present);
}

#[test]
#[ignore = "requires a disposable Docker daemon"]
fn partial_cleanup_preserves_foreign_resources_and_other_instances() {
    let observed = exercise_partial_cleanup().unwrap();
    assert!(!observed.report.verified());
    assert!(observed.recovery_present);
    assert!(observed.foreign_unchanged);
    assert!(observed.other_instance_unchanged);
}
