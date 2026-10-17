use super::*;

impl Runtime {
    pub fn stop(&self, name: &InstanceName) -> Result<()> {
        let report = self.verified_cleanup(name)?;
        if report.verified() {
            println!("Stopped and deleted {name} and all of its development data.");
            Ok(())
        } else {
            report.into_result()
        }
    }

    pub fn reset(&self, name: &InstanceName, force: bool) -> Result<()> {
        if !force {
            bail!("reset deletes chain, wallet, and seed data; repeat with --force");
        }
        let report = self.verified_cleanup(name)?;
        if report.verified() {
            println!("Deleted {name}; its Docker volumes cannot be recovered.");
            Ok(())
        } else {
            report.into_result()
        }
    }

    pub(super) fn verified_cleanup(&self, name: &InstanceName) -> Result<CleanupReport> {
        let policy = LifecyclePolicy::default();
        self.verified_cleanup_with(name, &policy, || {
            let context = LifecycleContext::new(policy);
            let deadline = Deadline::after(policy.cleanup);
            let docker = lifecycle_docker(&context, deadline, Cancellation::Ignore);
            self.cleanup_core(name, &docker, false, deadline, &context)
        })
    }

    #[cfg(test)]
    pub(super) fn delete_instance_resources_with(
        &self,
        name: &InstanceName,
        docker: &impl DockerResourceCommands,
    ) -> Result<()> {
        self.cleanup_with(name, docker, false, &LifecyclePolicy::default())?
            .into_result()
    }

    #[cfg(test)]
    pub(super) fn delete_instance_resources_with_mode(
        &self,
        name: &InstanceName,
        docker: &impl DockerResourceCommands,
        partial: bool,
    ) -> Result<()> {
        self.cleanup_with(name, docker, partial, &LifecyclePolicy::default())?
            .into_result()
    }

    #[cfg(test)]
    pub(super) fn cleanup_with(
        &self,
        name: &InstanceName,
        docker: &impl DockerResourceCommands,
        partial: bool,
        policy: &LifecyclePolicy,
    ) -> Result<CleanupReport> {
        let deadline = Deadline::after(policy.cleanup);
        let context = LifecycleContext::new(*policy);
        self.cleanup_core(name, docker, partial, deadline, &context)
    }

    pub(super) fn cleanup_core(
        &self,
        name: &InstanceName,
        docker: &impl DockerResourceCommands,
        partial: bool,
        deadline: Deadline,
        context: &LifecycleContext,
    ) -> Result<CleanupReport> {
        let recovery_path = self.recovery_path(name);
        let loaded = match recovery::RecoveryJournal::load(recovery_path.clone(), name) {
            Ok(journal) => journal,
            Err(error) => {
                let mut failures = vec![format!("{error:#}")];
                failures.extend(
                    context
                        .finish_helpers(deadline)
                        .iter()
                        .map(ToString::to_string),
                );
                return Ok(CleanupReport {
                    outcome: CleanupOutcome::Uncertain,
                    failures,
                    recovery_path: Some(recovery_path),
                    helpers_finished: context.helpers_finished(),
                });
            }
        };
        let mut journal = loaded;
        let kept = journal
            .as_ref()
            .map(|existing| retained_mutation_resources(existing.record(), false))
            .unwrap_or_default();
        let work_deadline = deadline.saturating_sub(context.policy.termination_reserve);
        let prefix = prefix(name);
        let mut failures = Vec::new();
        let mut uncertain = work_deadline.expired();
        if let Some(existing) = &journal
            && !existing.record().unresolved_helpers.is_empty()
        {
            uncertain = true;
            failures.push(format!(
                "previous launcher helpers remain unverified: {}",
                existing.record().unresolved_helpers.join(", ")
            ));
        }
        if work_deadline.expired() {
            failures.push(format!(
                "cleanup deadline for {name} expired before resource removal"
            ));
        }
        let mut targets: Vec<(String, String, String)> = Vec::new();
        let mut absent_containers = Vec::new();
        let mut consider = |kind: &str, target: &str| -> Result<()> {
            if kept.contains(&(resource_kind(kind), target.to_owned())) {
                let present = match resource_absent(docker, kind, target) {
                    Ok(absent) => !absent,
                    Err(error) => {
                        uncertain = true;
                        failures.push(format!(
                            "kept {kind} {target} but its snapshot failed: {error:#}"
                        ));
                        return Ok(());
                    }
                };
                let recovery::MutationOutcome::Uncertain(reason) =
                    recovery::reconcile_unidentified_create(present)
                else {
                    uncertain = true;
                    failures.push(format!("kept {kind} {target} without an uncertain outcome"));
                    return Ok(());
                };
                failures.push(format!("kept {kind} {target}: {reason}"));
                uncertain = true;
                return Ok(());
            }
            if work_deadline.expired() {
                uncertain = true;
                failures.push(format!("stopped before inspecting {kind} {target}"));
                return Ok(());
            }
            if let Some(id) = journal.as_ref().and_then(|existing| {
                existing
                    .record()
                    .mutations
                    .iter()
                    .find(|mutation| {
                        unresolved_allocation(mutation)
                            && mutation.resource.kind == resource_kind(kind)
                            && mutation.resource.name == target
                    })
                    .and_then(|mutation| mutation.resource.identity.clone())
                    .or_else(|| {
                        existing
                            .record()
                            .resources
                            .iter()
                            .find(|resource| {
                                resource.kind == resource_kind(kind) && resource.name == target
                            })
                            .and_then(|resource| resource.identity.clone())
                    })
            }) {
                let absent = match docker.output(&[kind, "inspect", &id]) {
                    Ok(details) if !details.trim().is_empty() => {
                        let recorded = recovery::ResourceRef {
                            kind: resource_kind(kind),
                            name: target.to_owned(),
                            identity: Some(id.clone()),
                        };
                        match inspected_resource_identity(kind, target, name, &details) {
                            Ok(observed) if recovery::identity_matches(&recorded, &observed) => {
                                targets.push((kind.to_owned(), id, target.to_owned()));
                                return Ok(());
                            }
                            Ok(observed) => {
                                uncertain = true;
                                failures.push(format!(
                                    "recorded {kind} {target} ({id}) does not match {observed}; refusing to adopt it"
                                ));
                                false
                            }
                            Err(error) => prove_recorded_absence(
                                docker,
                                kind,
                                target,
                                &id,
                                &format!("ownership inspection failed: {error:#}"),
                                &mut uncertain,
                                &mut failures,
                            ),
                        }
                    }
                    Ok(_) => prove_recorded_absence(
                        docker,
                        kind,
                        target,
                        &id,
                        "returned an empty inspection",
                        &mut uncertain,
                        &mut failures,
                    ),
                    Err(error) => prove_recorded_absence(
                        docker,
                        kind,
                        target,
                        &id,
                        &format!("could not be inspected: {error:#}"),
                        &mut uncertain,
                        &mut failures,
                    ),
                };
                if absent && kind == "container" {
                    absent_containers.push(recovery::ResourceRef {
                        kind: recovery::ResourceKind::Container,
                        name: target.to_owned(),
                        identity: Some(id),
                    });
                }
                return Ok(());
            }
            let result = owned_resource(docker, kind, target, name)
                .with_context(|| format!("{kind} {target}"));
            match result {
                Ok(Some(id)) => targets.push((kind.to_owned(), id, target.to_owned())),
                Ok(None) => {}
                Err(error) if partial => failures.push(format!("{error:#}")),
                Err(error) => return Err(error),
            }
            Ok(())
        };
        let preflight: Result<()> = (|| {
            for service in ["app", "lightwalletd", "zakura", "init"] {
                consider("container", &format!("{prefix}-{service}"))?;
            }
            for suffix in ["chain", "wallet", "lightwalletd", "config"] {
                consider("volume", &format!("{prefix}-{suffix}"))?;
            }
            consider("network", &prefix)?;
            Ok(())
        })();
        let preflight = preflight.and_then(|()| {
            if journal.is_none() && !targets.is_empty() {
                journal = Some(recovery::RecoveryJournal::create(
                    recovery_path.clone(),
                    name,
                )?);
            }
            Ok(())
        });
        if let Err(error) = preflight {
            failures.push(format!("{error:#}"));
            uncertain = true;
        } else {
            for resource in absent_containers {
                if work_deadline.expired() {
                    uncertain = true;
                    failures.push(format!(
                        "cleanup deadline expired before reconciling {}",
                        resource.name
                    ));
                    break;
                }
                if let Some(existing) = &mut journal
                    && let Err(error) = reconcile_absent_container(existing, docker, &resource)
                {
                    uncertain = true;
                    failures.push(format!("reconciling absent {}: {error:#}", resource.name));
                }
            }
            for (kind, id, label) in targets {
                // Ownership preflight includes dependencies, but deletion waits for
                // every referencing create or start to be durably reconciled.
                if kind != "container"
                    && journal.as_ref().is_some_and(|existing| {
                        retained_mutation_resources(existing.record(), true)
                            .contains(&(resource_kind(&kind), label.clone()))
                    })
                {
                    uncertain = true;
                    failures.push(format!(
                        "kept {kind} {label}: unresolved mutation dependencies"
                    ));
                    continue;
                }
                if work_deadline.expired() {
                    uncertain = true;
                    failures.push(format!("cleanup deadline expired before removing {label}"));
                    break;
                }
                if let Some(existing) = &mut journal {
                    let resource = recovery::ResourceRef {
                        kind: resource_kind(&kind),
                        name: label.clone(),
                        identity: Some(id.clone()),
                    };
                    if let Err(error) =
                        existing.begin(resource, Vec::new(), recovery::MutationOperation::Remove)
                    {
                        failures.push(format!(
                            "could not record removal of {label} before mutation: {error:#}"
                        ));
                        uncertain = true;
                        continue;
                    }
                }
                let removal = match kind.as_str() {
                    "container" => docker.run(&["rm", "-f", &id]),
                    "volume" => docker.run(&["volume", "rm", &id]),
                    _ => docker.run(&["network", "rm", &id]),
                };
                let mut reconciled_diagnostic = None;
                let recovering_container = kind == "container"
                    && journal.as_ref().is_some_and(|existing| {
                        existing.record().mutations.iter().any(|mutation| {
                            unresolved_allocation(mutation)
                                && mutation.resource.name == label
                                && mutation.resource.identity.as_deref() == Some(id.as_str())
                        })
                    });
                let absence = if recovering_container {
                    recorded_identity_absent(docker, &kind, &id)
                } else {
                    resource_absent(docker, &kind, &label)
                };
                let outcome = match absence {
                    Ok(true) => {
                        if let Err(error) = &removal {
                            reconciled_diagnostic = Some(format!(
                                "{label}: {error:#}; absence verified after lost removal reply"
                            ));
                        }
                        let unresolved = journal.as_ref().is_some_and(|existing| {
                            recovery::unresolved_create(
                                existing.record(),
                                &resource_kind(&kind),
                                &label,
                            )
                            .is_some()
                        });
                        let outcome = recovery::reconcile_acknowledged_removal(
                            true,
                            unresolved && !recovering_container,
                        );
                        if let recovery::MutationOutcome::Uncertain(reason) = &outcome {
                            uncertain = true;
                            failures.push(format!("{label}: {reason}"));
                        }
                        outcome
                    }
                    Ok(false) => match removal {
                        Ok(()) => {
                            failures.push(format!("{label} is still present after removal"));
                            recovery::MutationOutcome::ReconciledPresent
                        }
                        Err(error) => {
                            uncertain = true;
                            let reason = format!("{label}: {error:#}");
                            failures.push(reason.clone());
                            recovery::MutationOutcome::Uncertain(reason)
                        }
                    },
                    Err(error) => {
                        uncertain = true;
                        let mut reason =
                            format!("{label} removal could not be verified: {error:#}");
                        if let Err(removal_error) = removal {
                            reason.push_str(&format!("; removal failed: {removal_error:#}"));
                        }
                        failures.push(reason.clone());
                        recovery::MutationOutcome::Uncertain(reason)
                    }
                };
                if let Some(existing) = &mut journal {
                    let index = existing.record().mutations.len() - 1;
                    if let Some(diagnostic) = reconciled_diagnostic
                        && let Err(error) = existing.retain_failures(vec![diagnostic], Vec::new())
                    {
                        uncertain = true;
                        failures.push(format!(
                            "recording reconciled removal of {label}: {error:#}"
                        ));
                    }
                    let absent = outcome == recovery::MutationOutcome::ReconciledAbsent;
                    if let Err(error) = existing.finish(index, outcome, Some(id.clone())) {
                        uncertain = true;
                        failures.push(format!("recording removal of {label}: {error:#}"));
                    } else if absent
                        && kind == "container"
                        && let Err(error) = reconcile_absent_container(
                            existing,
                            docker,
                            &recovery::ResourceRef {
                                kind: recovery::ResourceKind::Container,
                                name: label.clone(),
                                identity: Some(id),
                            },
                        )
                    {
                        uncertain = true;
                        failures.push(format!("reconciling absent {label}: {error:#}"));
                    }
                }
            }
        }
        if journal.as_ref().is_some_and(|existing| {
            !retained_mutation_resources(existing.record(), true).is_empty()
        }) {
            uncertain = true;
        }
        for failure in context.finish_helpers(deadline) {
            uncertain = true;
            failures.push(failure.to_string());
        }
        let helpers_finished = context.helpers_finished();
        if !helpers_finished {
            uncertain = true;
            failures.push(format!(
                "unresolved helpers remain: {}",
                context.unresolved_helpers().join(", ")
            ));
        }
        let outcome = if uncertain {
            CleanupOutcome::Uncertain
        } else if failures.is_empty() {
            CleanupOutcome::VerifiedComplete
        } else {
            CleanupOutcome::Incomplete
        };
        if matches!(outcome, CleanupOutcome::VerifiedComplete) && helpers_finished {
            let dir = self.instance_dir(name);
            if dir.exists()
                && let Err(error) = fs::remove_dir_all(&dir)
            {
                failures.push(format!("removing metadata {}: {error}", dir.display()));
                if let Some(existing) = &mut journal
                    && let Err(error) =
                        existing.retain_failures(failures.clone(), context.unresolved_helpers())
                {
                    failures.push(format!("retaining cleanup recovery: {error:#}"));
                }
                return Ok(CleanupReport {
                    outcome: CleanupOutcome::Incomplete,
                    failures,
                    recovery_path: Some(recovery_path),
                    helpers_finished,
                });
            }
            return Ok(CleanupReport {
                outcome: CleanupOutcome::VerifiedComplete,
                failures,
                recovery_path: None,
                helpers_finished,
            });
        }
        if let Some(existing) = &mut journal
            && let Err(error) =
                existing.retain_failures(failures.clone(), context.unresolved_helpers())
        {
            failures.push(format!("retaining cleanup recovery: {error:#}"));
        }
        Ok(CleanupReport {
            outcome,
            failures,
            recovery_path: Some(recovery_path),
            helpers_finished,
        })
    }
}

#[derive(Debug)]
pub(super) enum CleanupOutcome {
    VerifiedComplete,
    Incomplete,
    Uncertain,
}

#[derive(Debug)]
pub(super) struct CleanupReport {
    pub(super) outcome: CleanupOutcome,
    pub(super) failures: Vec<String>,
    pub(super) recovery_path: Option<PathBuf>,
    pub(super) helpers_finished: bool,
}

impl CleanupReport {
    pub(super) fn verified(&self) -> bool {
        matches!(self.outcome, CleanupOutcome::VerifiedComplete) && self.helpers_finished
    }

    pub(super) fn into_result(self) -> Result<()> {
        if self.verified() {
            Ok(())
        } else if self.failures.is_empty() {
            bail!("cleanup was not verified")
        } else {
            let location = self
                .recovery_path
                .as_ref()
                .map(|path| format!("; recovery remains at {}", path.display()))
                .unwrap_or_default();
            bail!("{}{location}", self.failures.join("; "))
        }
    }
}

/// A failed or empty inspect is not absence. A completed identity listing that
/// does not contain the recorded id is.
pub(super) fn prove_recorded_absence(
    docker: &impl DockerResourceCommands,
    kind: &str,
    target: &str,
    id: &str,
    reason: &str,
    uncertain: &mut bool,
    failures: &mut Vec<String>,
) -> bool {
    match recorded_identity_absent(docker, kind, id) {
        Ok(true) => true,
        Ok(false) => {
            *uncertain = true;
            failures.push(format!("recorded {kind} {target} ({id}) {reason}"));
            false
        }
        Err(error) => {
            *uncertain = true;
            failures.push(format!(
                "recorded {kind} {target} ({id}) {reason}; identity listing failed: {error:#}"
            ));
            false
        }
    }
}

pub(super) fn recorded_identity_absent(
    docker: &impl DockerResourceCommands,
    kind: &str,
    identity: &str,
) -> Result<bool> {
    let listing = match kind {
        "container" => {
            docker.output(&["container", "ls", "-a", "--no-trunc", "--format", "{{.ID}}"])?
        }
        "network" => docker.output(&["network", "ls", "--no-trunc", "--format", "{{.ID}}"])?,
        "volume" => docker.output(&["volume", "ls", "--format", "{{.Name}}"])?,
        _ => unreachable!("resource kind is fixed by the caller"),
    };
    Ok(!listing
        .lines()
        .any(|candidate| candidate.trim() == identity))
}

pub(super) fn resource_absent(
    docker: &impl DockerResourceCommands,
    kind: &str,
    name: &str,
) -> Result<bool> {
    let names = match kind {
        "container" => docker.output(&["container", "ls", "-a", "--format", "{{.Names}}"])?,
        "volume" => docker.output(&["volume", "ls", "--format", "{{.Name}}"])?,
        "network" => docker.output(&["network", "ls", "--format", "{{.Name}}"])?,
        _ => unreachable!("resource kind is fixed by the caller"),
    };
    Ok(!names.lines().any(|candidate| candidate.trim() == name))
}

pub(super) fn unresolved_allocation(mutation: &recovery::MutationRecord) -> bool {
    matches!(
        mutation.operation,
        recovery::MutationOperation::Create | recovery::MutationOperation::Start
    ) && matches!(
        mutation.outcome,
        recovery::MutationOutcome::Pending | recovery::MutationOutcome::Uncertain(_)
    )
}

pub(super) fn retained_mutation_resources(
    record: &recovery::RecoveryRecord,
    include_known_containers: bool,
) -> std::collections::HashSet<(recovery::ResourceKind, String)> {
    let mut kept = std::collections::HashSet::new();
    for mutation in &record.mutations {
        let resource = &mutation.resource;
        let known_container = resource.kind == recovery::ResourceKind::Container
            && resource
                .identity
                .as_deref()
                .is_some_and(|id| !id.is_empty())
            && !record.resources.iter().any(|other| {
                other.kind == resource.kind
                    && other.name == resource.name
                    && other.identity.is_some()
                    && other.identity != resource.identity
            })
            && !record.mutations.iter().any(|other| {
                unresolved_allocation(other)
                    && other.resource.kind == resource.kind
                    && other.resource.name == resource.name
                    && other.resource.identity != resource.identity
            });
        if unresolved_allocation(mutation) && (include_known_containers || !known_container) {
            kept.insert((resource.kind.clone(), resource.name.clone()));
            for dependency in &mutation.dependencies {
                kept.insert((dependency.kind.clone(), dependency.name.clone()));
            }
        }
    }
    kept
}

pub(super) fn reconcile_absent_container(
    journal: &mut recovery::RecoveryJournal,
    docker: &impl DockerResourceCommands,
    resource: &recovery::ResourceRef,
) -> Result<()> {
    if !journal
        .record()
        .mutations
        .iter()
        .any(|mutation| unresolved_allocation(mutation) && mutation.resource == *resource)
    {
        return Ok(());
    }
    // Do not release dependencies underneath a same-name replacement.
    if !resource_absent(docker, "container", &resource.name)? {
        bail!(
            "same-name container {} remains; refusing to release allocation dependencies",
            resource.name
        );
    }
    journal.reconcile_absent_allocations(resource)
}
