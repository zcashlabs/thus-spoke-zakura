//! Versioned recovery journal for one instance directory.
//!
//! The file is `<instance_dir>/lifecycle-recovery.json`, format version 1. It is not
//! `instance.json` and it is not a session lock. A missing file is an older instance.
//! Invalid, wrong-version, or wrong-name records fail before any destructive recovery.
//! A failed rewrite leaves the previous valid file in place.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::InstanceName;

pub(super) const RECOVERY_VERSION: u32 = 1;
pub(super) const RECOVERY_FILE: &str = "lifecycle-recovery.json";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub(super) enum ResourceKind {
    Container,
    Volume,
    Network,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ResourceRef {
    pub(super) kind: ResourceKind,
    pub(super) name: String,
    pub(super) identity: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) enum MutationOperation {
    Create,
    Start,
    Remove,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) enum MutationOutcome {
    Pending,
    Acknowledged,
    Failed(String),
    Uncertain(String),
    ReconciledPresent,
    ReconciledAbsent,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct MutationRecord {
    pub(super) resource: ResourceRef,
    pub(super) dependencies: Vec<ResourceRef>,
    pub(super) operation: MutationOperation,
    pub(super) outcome: MutationOutcome,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct RecoveryRecord {
    pub(super) version: u32,
    pub(super) instance: String,
    pub(super) resources: Vec<ResourceRef>,
    pub(super) mutations: Vec<MutationRecord>,
    pub(super) failures: Vec<String>,
    pub(super) unresolved_helpers: Vec<String>,
}

pub(super) struct RecoveryJournal {
    path: PathBuf,
    record: RecoveryRecord,
    #[cfg(test)]
    fail_next_write: bool,
}

impl RecoveryJournal {
    pub(super) fn create(path: PathBuf, name: &InstanceName) -> Result<Self> {
        let journal = Self {
            path,
            record: RecoveryRecord {
                version: RECOVERY_VERSION,
                instance: name.to_string(),
                resources: Vec::new(),
                mutations: Vec::new(),
                failures: Vec::new(),
                unresolved_helpers: Vec::new(),
            },
            #[cfg(test)]
            fail_next_write: false,
        };
        journal.store()?;
        Ok(journal)
    }

    pub(super) fn load(path: PathBuf, name: &InstanceName) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)
            .with_context(|| format!("reading recovery journal {}", path.display()))?;
        let record: RecoveryRecord = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "invalid recovery journal {}; refusing destructive recovery",
                path.display()
            )
        })?;
        if record.version != RECOVERY_VERSION {
            bail!(
                "recovery journal {} has version {}, expected {RECOVERY_VERSION}; refusing destructive recovery",
                path.display(),
                record.version
            );
        }
        if record.instance != name.to_string() {
            bail!(
                "recovery journal {} belongs to {}, not {name}; refusing destructive recovery",
                path.display(),
                record.instance
            );
        }
        for resource in record
            .resources
            .iter()
            .chain(record.mutations.iter().map(|mutation| &mutation.resource))
        {
            if resource.kind == ResourceKind::Volume
                && resource
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity != &resource.name)
            {
                bail!(
                    "recovery journal {} has an invalid volume identity for {}; refusing destructive recovery",
                    path.display(),
                    resource.name
                );
            }
        }
        Ok(Some(Self {
            path,
            record,
            #[cfg(test)]
            fail_next_write: false,
        }))
    }

    pub(super) fn begin(
        &mut self,
        resource: ResourceRef,
        dependencies: Vec<ResourceRef>,
        operation: MutationOperation,
    ) -> Result<usize> {
        let mut updated = self.record.clone();
        updated.mutations.push(MutationRecord {
            resource,
            dependencies,
            operation,
            outcome: MutationOutcome::Pending,
        });
        self.persist(updated)?;
        Ok(self.record.mutations.len() - 1)
    }

    pub(super) fn finish(
        &mut self,
        index: usize,
        outcome: MutationOutcome,
        identity: Option<String>,
    ) -> Result<()> {
        let mut updated = self.record.clone();
        let resource = {
            let mutation = updated
                .mutations
                .get_mut(index)
                .with_context(|| format!("recovery mutation {index} is missing"))?;
            mutation.outcome = outcome;
            if let Some(identity) = &identity {
                mutation.resource.identity = Some(identity.clone());
            }
            mutation.resource.clone()
        };
        remember_resource(&mut updated.resources, &resource, identity);
        self.persist(updated)
    }

    pub(super) fn record(&self) -> &RecoveryRecord {
        &self.record
    }

    pub(super) fn retain_failures(
        &mut self,
        failures: Vec<String>,
        helpers: Vec<String>,
    ) -> Result<()> {
        let mut updated = self.record.clone();
        updated.failures.extend(failures);
        updated.unresolved_helpers.extend(helpers);
        self.persist(updated)
    }

    #[cfg(test)]
    pub(super) fn fail_next_write(&mut self) {
        self.fail_next_write = true;
    }

    fn persist(&mut self, updated: RecoveryRecord) -> Result<()> {
        #[cfg(test)]
        if self.fail_next_write {
            self.fail_next_write = false;
            bail!(
                "recovery journal {} could not be rewritten",
                self.path.display()
            );
        }
        let previous = self.record.clone();
        self.record = updated;
        if let Err(error) = self.store() {
            self.record = previous;
            return Err(error);
        }
        Ok(())
    }

    fn store(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let temporary = self.path.with_extension("json.tmp");
        let payload =
            serde_json::to_vec_pretty(&self.record).context("encoding recovery journal")?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.write_all(&payload)
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", temporary.display()))?;
        fs::rename(&temporary, &self.path).with_context(|| {
            format!(
                "replacing {} with {}",
                self.path.display(),
                temporary.display()
            )
        })?;
        Ok(())
    }
}

fn remember_resource(
    resources: &mut Vec<ResourceRef>,
    resource: &ResourceRef,
    identity: Option<String>,
) {
    if let Some(existing) = resources
        .iter_mut()
        .find(|item| item.kind == resource.kind && item.name == resource.name)
    {
        if identity.is_some() {
            existing.identity = identity;
        }
        return;
    }
    let mut stored = resource.clone();
    if identity.is_some() {
        stored.identity = identity;
    }
    resources.push(stored);
}

/// A pending record reloaded after a crash is uncertainty, not an acknowledgement.
pub(super) fn loaded_outcome(outcome: &MutationOutcome) -> MutationOutcome {
    match outcome {
        MutationOutcome::Pending => MutationOutcome::Uncertain(
            "pending mutation was not acknowledged before the launcher exited".into(),
        ),
        other => other.clone(),
    }
}

pub(super) fn unresolved_create<'a>(
    record: &'a RecoveryRecord,
    kind: &ResourceKind,
    resource_name: &str,
) -> Option<&'a MutationRecord> {
    record.mutations.iter().rev().find(|mutation| {
        mutation.resource.kind == *kind
            && mutation.resource.name == resource_name
            && matches!(
                mutation.operation,
                MutationOperation::Create | MutationOperation::Start
            )
            && matches!(
                mutation.outcome,
                MutationOutcome::Pending | MutationOutcome::Uncertain(_)
            )
    })
}

/// Original identity wins. A same-name or same-label object with a different id is not adopted.
pub(super) fn identity_matches(recorded: &ResourceRef, observed_id: &str) -> bool {
    match &recorded.identity {
        Some(identity) => identity == observed_id,
        None => false,
    }
}

/// Timed-out create without an id stays uncertain whether the name is absent or present.
pub(super) fn reconcile_unidentified_create(present: bool) -> MutationOutcome {
    MutationOutcome::Uncertain(if present {
        "create was not acknowledged and a same-name resource is present".into()
    } else {
        "create was not acknowledged and the resource was absent".into()
    })
}

pub(super) fn reconcile_acknowledged_removal(
    absent: bool,
    unresolved_create_remains: bool,
) -> MutationOutcome {
    if unresolved_create_remains {
        return MutationOutcome::Uncertain(
            "removal was observed but an unresolved create still references the resource".into(),
        );
    }
    if absent {
        MutationOutcome::ReconciledAbsent
    } else {
        MutationOutcome::ReconciledPresent
    }
}
