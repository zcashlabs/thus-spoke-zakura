use super::*;

pub(super) const INSTANCE_LABEL: &str = "com.zakura.ths.instance";

pub(super) struct AllocatedInstance {
    pub(super) endpoints: Endpoints,
    pub(super) resources: Vec<recovery::ResourceRef>,
    pub(super) app_container: WatchedContainer,
    pub(super) zakura_container_id: String,
}

pub(super) fn resource_kind(kind: &str) -> recovery::ResourceKind {
    match kind {
        "container" => recovery::ResourceKind::Container,
        "volume" => recovery::ResourceKind::Volume,
        "network" => recovery::ResourceKind::Network,
        _ => unreachable!("resource kind is fixed by the caller"),
    }
}

pub(super) fn inspected_resource_identity(
    kind: &str,
    target: &str,
    name: &InstanceName,
    details: &str,
) -> Result<String> {
    let resources: serde_json::Value = serde_json::from_str(details)
        .with_context(|| format!("decoding Docker {kind} {target}"))?;
    let items = resources
        .as_array()
        .with_context(|| format!("Docker returned invalid {kind} {target}"))?;
    if items.len() != 1 {
        bail!(
            "Docker returned {} {kind} resources named {target}",
            items.len()
        );
    }
    let resource = &items[0];
    let labels = if kind == "container" {
        &resource["Config"]["Labels"]
    } else {
        &resource["Labels"]
    };
    if labels[INSTANCE_LABEL].as_str() != Some(name.0.as_str()) {
        bail!(
            "Docker {kind} {target} is not owned by ths instance {name}; refusing to reuse or delete it"
        );
    }
    // Containers and networks have stable IDs; Docker identifies volumes by name.
    let id_field = if kind == "volume" { "Name" } else { "Id" };
    let id = resource[id_field]
        .as_str()
        .filter(|id| !id.is_empty())
        .with_context(|| format!("Docker {kind} {target} has no {id_field}"))?;
    Ok(id.to_owned())
}

pub(super) fn owned_resource(
    docker: &impl DockerResourceCommands,
    kind: &str,
    target: &str,
    name: &InstanceName,
) -> Result<Option<String>> {
    let names = match kind {
        "container" => docker.output(&["container", "ls", "-a", "--format", "{{.Names}}"])?,
        "volume" => docker.output(&["volume", "ls", "--format", "{{.Name}}"])?,
        "network" => docker.output(&["network", "ls", "--format", "{{.Name}}"])?,
        _ => unreachable!("resource kind is fixed by the caller"),
    };
    let matches = names
        .lines()
        .filter(|candidate| candidate.trim() == target)
        .count();
    if matches == 0 {
        return Ok(None);
    }
    if matches > 1 {
        bail!(
            "Docker has multiple {kind} resources named {target}; refusing to reuse or delete them"
        );
    }
    let details = docker.output(&[kind, "inspect", target])?;
    inspected_resource_identity(kind, target, name, &details).map(Some)
}

pub(super) fn mutation_failure(
    journal: &mut recovery::RecoveryJournal,
    index: usize,
    error: anyhow::Error,
    identity: Option<String>,
) -> anyhow::Error {
    match journal.finish(
        index,
        recovery::MutationOutcome::Uncertain(format!("{error:#}")),
        identity,
    ) {
        Ok(()) => error,
        Err(record_error) => anyhow!("{error:#}; recording mutation uncertainty: {record_error:#}"),
    }
}

pub(super) fn ensure_network_with(
    prefix: &str,
    name: &InstanceName,
    docker: &impl DockerResourceCommands,
    journal: &mut recovery::RecoveryJournal,
    deadline: Deadline,
) -> Result<recovery::ResourceRef> {
    bounded_ensure(deadline, prefix, || {
        if let Some(id) = owned_resource(docker, "network", prefix, name)? {
            return Ok(recovery::ResourceRef {
                kind: recovery::ResourceKind::Network,
                name: prefix.to_owned(),
                identity: Some(id),
            });
        }
        let resource = recovery::ResourceRef {
            kind: recovery::ResourceKind::Network,
            name: prefix.to_owned(),
            identity: None,
        };
        let index = journal.begin(
            resource.clone(),
            Vec::new(),
            recovery::MutationOperation::Create,
        )?;
        if let Err(error) = docker.run(&["network", "create", "--label", &label(name), prefix]) {
            return Err(mutation_failure(journal, index, error, None));
        }
        match owned_resource(docker, "network", prefix, name)? {
            Some(id) => {
                journal.finish(
                    index,
                    recovery::MutationOutcome::Acknowledged,
                    Some(id.clone()),
                )?;
                Ok(recovery::ResourceRef {
                    identity: Some(id),
                    ..resource
                })
            }
            None => Err(mutation_failure(
                journal,
                index,
                anyhow!("Docker did not create network {prefix}"),
                None,
            )),
        }
    })
}
pub(super) fn ensure_volume_with(
    volume: &str,
    name: &InstanceName,
    docker: &impl DockerResourceCommands,
    journal: &mut recovery::RecoveryJournal,
    deadline: Deadline,
) -> Result<recovery::ResourceRef> {
    bounded_ensure(deadline, volume, || {
        if let Some(id) = owned_resource(docker, "volume", volume, name)? {
            return Ok(recovery::ResourceRef {
                kind: recovery::ResourceKind::Volume,
                name: volume.to_owned(),
                identity: Some(id),
            });
        }
        let resource = recovery::ResourceRef {
            kind: recovery::ResourceKind::Volume,
            name: volume.to_owned(),
            identity: None,
        };
        let index = journal.begin(
            resource.clone(),
            Vec::new(),
            recovery::MutationOperation::Create,
        )?;
        if let Err(error) = docker.run(&["volume", "create", "--label", &label(name), volume]) {
            return Err(mutation_failure(journal, index, error, None));
        }
        match owned_resource(docker, "volume", volume, name)? {
            Some(id) => {
                journal.finish(
                    index,
                    recovery::MutationOutcome::Acknowledged,
                    Some(id.clone()),
                )?;
                Ok(recovery::ResourceRef {
                    identity: Some(id),
                    ..resource
                })
            }
            None => Err(mutation_failure(
                journal,
                index,
                anyhow!("Docker did not create volume {volume}"),
                None,
            )),
        }
    })
}

pub(super) fn bounded_ensure<T>(
    deadline: Deadline,
    resource: &str,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if deadline.expired() {
        bail!("timed out before creating {resource}");
    }
    operation()
}
pub(super) fn volume_dependency(
    journal: &recovery::RecoveryJournal,
    target: &str,
) -> Result<recovery::ResourceRef> {
    journal
        .record()
        .resources
        .iter()
        .find(|resource| resource.kind == recovery::ResourceKind::Volume && resource.name == target)
        .cloned()
        .with_context(|| format!("missing allocated dependency {target}"))
}

pub(super) fn ensure_zakura(
    prefix: &str,
    name: &InstanceName,
    ports: &HostPorts,
    docker: &impl DockerResourceCommands,
    journal: &mut recovery::RecoveryJournal,
    deadline: Deadline,
    mut dependencies: Vec<recovery::ResourceRef>,
) -> Result<String> {
    for suffix in ["chain", "config"] {
        dependencies.push(volume_dependency(journal, &format!("{prefix}-{suffix}"))?);
    }

    let rpc_bind = loopback_publish(ports.rpc, 18232);
    let p2p_bind = loopback_publish(ports.p2p, 18233);
    create_container(
        &format!("{prefix}-zakura"),
        name,
        docker,
        journal,
        deadline,
        dependencies,
        &[
            "create",
            "--name",
            &format!("{prefix}-zakura"),
            "--network",
            prefix,
            "--network-alias",
            "zakura",
            "--label",
            &label(name),
            "-p",
            &rpc_bind,
            "-p",
            &p2p_bind,
            "-v",
            &format!("{prefix}-chain:/data"),
            "-v",
            &format!("{prefix}-config:/config:ro"),
            "-e",
            "CONFIG_FILE_PATH=/config/zakurad.toml",
            "-e",
            "ZAKURA_STATE__CACHE_DIR=/data",
            ZAKURA_IMAGE,
            "zakurad",
            "start",
        ],
    )
}
pub(super) fn ensure_lightwalletd(
    prefix: &str,
    name: &InstanceName,
    ports: &HostPorts,
    docker: &impl DockerResourceCommands,
    journal: &mut recovery::RecoveryJournal,
    deadline: Deadline,
    mut dependencies: Vec<recovery::ResourceRef>,
) -> Result<String> {
    dependencies.push(volume_dependency(
        journal,
        &format!("{prefix}-lightwalletd"),
    )?);

    let image = lightwalletd_image();
    let lightwalletd_bind = loopback_publish(ports.lightwalletd, 9067);
    let target = format!("{prefix}-lightwalletd");
    create_container(
        &target,
        name,
        docker,
        journal,
        deadline,
        dependencies,
        &[
            "create",
            "--name",
            &target,
            "--network",
            prefix,
            "--network-alias",
            "lightwalletd",
            "--label",
            &label(name),
            "--user",
            "0:0",
            "-p",
            &lightwalletd_bind,
            "-v",
            &format!("{prefix}-lightwalletd:/var/lib/lightwalletd"),
            &image,
            "--no-tls-very-insecure",
            "--grpc-bind-addr",
            "0.0.0.0:9067",
            "--rpchost",
            "zakura",
            "--rpcport",
            "18232",
            "--rpcuser",
            "unused",
            "--rpcpassword",
            "unused",
            "--data-dir",
            "/var/lib/lightwalletd",
            "--log-file",
            "/dev/stdout",
        ],
    )
}
#[allow(clippy::too_many_arguments)]
pub(super) fn ensure_app(
    prefix: &str,
    name: &InstanceName,
    ports: &HostPorts,
    docker: &impl DockerResourceCommands,
    journal: &mut recovery::RecoveryJournal,
    deadline: Deadline,
    mut dependencies: Vec<recovery::ResourceRef>,
    mining: MiningConfig,
) -> Result<WatchedContainer> {
    dependencies.push(volume_dependency(journal, &format!("{prefix}-wallet"))?);

    let public_rpc = format!("http://127.0.0.1:{}", ports.rpc);
    let public_lightwalletd = format!("http://127.0.0.1:{}", ports.lightwalletd);
    let public_p2p = format!("127.0.0.1:{}", ports.p2p);
    let public_dashboard = format!("http://127.0.0.1:{}", ports.dashboard);
    let dashboard_bind = loopback_publish(ports.dashboard, 8080);
    let image = app_image();
    let target = format!("{prefix}-app");
    let instance = format!("THS_INSTANCE={name}");
    let public_rpc_env = format!("THS_PUBLIC_ZAKURA_RPC={public_rpc}");
    let public_lightwalletd_env = format!("THS_PUBLIC_LIGHTWALLETD={public_lightwalletd}");
    let public_p2p_env = format!("THS_PUBLIC_P2P={public_p2p}");
    let public_dashboard_env = format!("THS_PUBLIC_DASHBOARD={public_dashboard}");
    let mining_mode_env = format!("THS_MINING_MODE={}", mining.mode.as_str());
    let block_time_env = mining
        .block_time
        .map(|seconds| format!("THS_BLOCK_TIME={seconds}"));
    let mut args: Vec<String> = vec![
        "create".into(),
        "--name".into(),
        target.clone(),
        "--network".into(),
        prefix.into(),
        "--label".into(),
        label(name),
        "-p".into(),
        dashboard_bind,
        "-e".into(),
        "THS_LISTEN=0.0.0.0:8080".into(),
        "-e".into(),
        "THS_ZAKURA_RPC=http://zakura:18232".into(),
        "-e".into(),
        "THS_LIGHTWALLETD=http://lightwalletd:9067".into(),
        "-e".into(),
        instance,
        "-e".into(),
        public_rpc_env,
        "-e".into(),
        public_lightwalletd_env,
        "-e".into(),
        public_p2p_env,
        "-e".into(),
        public_dashboard_env,
        "-e".into(),
        mining_mode_env,
    ];
    if let Some(block_time_env) = block_time_env {
        args.push("-e".into());
        args.push(block_time_env);
    }
    args.extend([
        "-v".into(),
        format!("{prefix}-wallet:/data"),
        image,
        "serve".into(),
        "--data-dir".into(),
        "/data".into(),
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let id = create_container(
        &target,
        name,
        docker,
        journal,
        deadline,
        dependencies,
        &args,
    )?;
    if id.is_empty() {
        bail!("Docker returned no ID for app container {target}");
    }
    // Read the replay cursor by allocated ID so a replacement cannot become the watched app.
    let created = docker.output(&["container", "inspect", "--format", "{{.Created}}", &id])?;
    if created.is_empty() {
        bail!("Docker returned no creation time for app container {id}");
    }
    Ok(WatchedContainer { id, created })
}

pub(super) fn create_container(
    target: &str,
    name: &InstanceName,
    docker: &impl DockerResourceCommands,
    journal: &mut recovery::RecoveryJournal,
    deadline: Deadline,
    dependencies: Vec<recovery::ResourceRef>,
    args: &[&str],
) -> Result<String> {
    if deadline.expired() {
        bail!("timed out before creating {target}");
    }
    if let Some(id) = owned_resource(docker, "container", target, name)? {
        return Ok(id);
    }
    let index = journal.begin(
        recovery::ResourceRef {
            kind: recovery::ResourceKind::Container,
            name: target.to_owned(),
            identity: None,
        },
        dependencies,
        recovery::MutationOperation::Create,
    )?;
    let created = docker.output(args);
    let id = match created {
        Ok(id) => id,
        Err(error) => {
            return Err(mutation_failure(journal, index, error, None));
        }
    };
    if let Err(error) = require_container_identity(docker, &id, name) {
        return Err(mutation_failure(journal, index, error, Some(id)));
    }
    journal.finish(
        index,
        recovery::MutationOutcome::Acknowledged,
        Some(id.clone()),
    )?;
    Ok(id)
}

pub(super) fn require_container_identity(
    docker: &impl DockerResourceCommands,
    id: &str,
    name: &InstanceName,
) -> Result<()> {
    let details = docker.output(&["container", "inspect", id])?;
    let resources: serde_json::Value = serde_json::from_str(&details)
        .with_context(|| format!("decoding Docker container {id}"))?;
    let items = resources
        .as_array()
        .with_context(|| format!("Docker returned invalid container {id}"))?;
    if items.len() != 1 {
        bail!(
            "Docker returned {} container resources for {id}; refusing to adopt a replacement",
            items.len()
        );
    }
    let resource = &items[0];
    if resource["Id"].as_str() != Some(id) {
        bail!("Docker container {id} changed identity; refusing to adopt a replacement");
    }
    if resource["Config"]["Labels"][INSTANCE_LABEL].as_str() != Some(name.0.as_str()) {
        bail!("Docker container {id} is not owned by ths instance {name}");
    }
    Ok(())
}

pub(super) fn start_container(
    id: &str,
    name: &str,
    docker: &impl DockerResourceCommands,
    journal: &mut recovery::RecoveryJournal,
    deadline: Deadline,
) -> Result<()> {
    if deadline.expired() {
        bail!("timed out before starting {name}");
    }
    let dependencies = journal
        .record()
        .mutations
        .iter()
        .rev()
        .find(|mutation| {
            mutation.operation == recovery::MutationOperation::Create
                && mutation.resource.name == name
        })
        .with_context(|| format!("missing creation record for {name}"))?
        .dependencies
        .clone();
    let index = journal.begin(
        recovery::ResourceRef {
            kind: recovery::ResourceKind::Container,
            name: name.to_owned(),
            identity: Some(id.to_owned()),
        },
        dependencies,
        recovery::MutationOperation::Start,
    )?;
    if let Err(error) = docker.run(&["start", id]) {
        return Err(mutation_failure(journal, index, error, Some(id.to_owned())));
    }
    journal.finish(
        index,
        recovery::MutationOutcome::Acknowledged,
        Some(id.to_owned()),
    )?;
    Ok(())
}

pub(super) fn inspect_endpoints(prefix: &str) -> Result<Endpoints> {
    Ok(Endpoints {
        dashboard: format!(
            "http://127.0.0.1:{}",
            published_port(&format!("{prefix}-app"), "8080/tcp")?
        ),
        rpc: format!(
            "http://127.0.0.1:{}",
            published_port(&format!("{prefix}-zakura"), "18232/tcp")?
        ),
        lightwalletd: format!(
            "http://127.0.0.1:{}",
            published_port(&format!("{prefix}-lightwalletd"), "9067/tcp")?
        ),
        p2p: format!(
            "127.0.0.1:{}",
            published_port(&format!("{prefix}-zakura"), "18233/tcp")?
        ),
        network: default_regtest(),
        tls: false,
    })
}
pub(super) fn published_port(container: &str, port: &str) -> Result<u16> {
    docker_output([
        "inspect",
        "--format",
        &format!("{{{{(index (index .NetworkSettings.Ports \"{port}\") 0).HostPort}}}}"),
        container,
    ])?
    .parse()
    .context("Docker returned an invalid published port")
}
/// Lists only running containers, so a stopped or missing one is `false` rather than an error.
pub(crate) fn container_running(name: &str) -> Result<bool> {
    docker_listed([
        "container",
        "ls",
        "--quiet",
        "--filter",
        &format!("name=^{name}$"),
    ])
}
pub(super) fn published_port_with(
    docker: &impl DockerResourceCommands,
    container: &str,
    port: &str,
) -> Result<u16> {
    docker
        .output(&[
            "inspect",
            "--format",
            &format!("{{{{(index (index .NetworkSettings.Ports \"{port}\") 0).HostPort}}}}"),
            container,
        ])?
        .parse()
        .context("Docker returned an invalid published port")
}

/// `docker inspect` fails the same way for a missing object and an unreachable daemon, so
/// existence comes from a filtered listing: empty means absent, failure stays an error.
pub(super) fn docker_listed<const N: usize>(args: [&str; N]) -> Result<bool> {
    Ok(!docker_output(args)?.is_empty())
}
