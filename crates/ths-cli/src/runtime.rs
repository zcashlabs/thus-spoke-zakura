#[cfg(test)]
use std::collections::BTreeMap;
use std::{
    cell::RefCell,
    ffi::OsString,
    fmt::{self, Display},
    fs::{self, File, OpenOptions, TryLockError},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    str::FromStr,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicI32, Ordering},
        mpsc::{self, RecvTimeoutError, TryRecvError},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::lifecycle::{
    Cancellation, CapturedOutput, CommandFailure, CommandFailureKind, Deadline, HelperSet,
    LifecyclePolicy, OutputMode, run, run_leaving_running_on_timeout,
};
#[cfg(test)]
use crate::lifecycle::{ChildOps, run_observed};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[cfg(test)]
#[path = "../tests/unit/runtime/docker_lifecycle_tests.rs"]
mod docker_lifecycle_tests;
#[cfg(test)]
#[path = "../tests/unit/runtime/lifecycle_tests.rs"]
mod lifecycle_tests;
mod readiness;
mod recovery;

#[cfg(test)]
use crate::{client::*, intents::*};
mod deletion;
use deletion::*;
mod cleanup;
use cleanup::*;
mod docker;
use docker::*;
mod images;
use images::*;
mod instance;
pub(crate) use instance::InstanceName;
pub(crate) use instance::prefix;
use instance::*;
mod logs;
#[cfg(test)]
use logs::*;
mod resources;
use crate::client::{RuntimeStatus, runtime_status};
pub(crate) use instance::Endpoints;
pub(crate) use resources::container_running;
use resources::*;
mod rpc;
mod runner;
use runner::*;
mod startup;
use startup::*;
mod watch;

pub struct Runtime {
    root: PathBuf,
}

/// The mining mode selected for a new environment. The server owns the actual
/// behavior; the launcher only validates the flags and forwards the choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MiningConfig {
    pub(crate) mode: MiningMode,
    pub(crate) block_time: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MiningMode {
    Auto,
    Interval,
    Mempool,
    Manual,
}

impl MiningMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Interval => "interval",
            Self::Mempool => "mempool",
            Self::Manual => "manual",
        }
    }
}

impl Default for MiningConfig {
    fn default() -> Self {
        Self {
            mode: MiningMode::Auto,
            block_time: None,
        }
    }
}

impl MiningConfig {
    pub(crate) fn from_flags(
        block_time: Option<u64>,
        mine_on_mempool: bool,
        no_auto_mine: bool,
    ) -> Self {
        match (block_time, mine_on_mempool, no_auto_mine) {
            (Some(seconds), _, _) => Self {
                mode: MiningMode::Interval,
                block_time: Some(seconds),
            },
            (None, true, _) => Self {
                mode: MiningMode::Mempool,
                block_time: None,
            },
            (None, false, true) => Self {
                mode: MiningMode::Manual,
                block_time: None,
            },
            (None, false, false) => Self::default(),
        }
    }
}

impl Runtime {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("com", "zakura", "thus-spoke-zakura")
            .ok_or_else(|| anyhow!("could not determine the platform configuration directory"))?;
        Ok(Self {
            root: dirs.config_dir().to_owned(),
        })
    }

    pub fn doctor(&self, json: bool) -> Result<()> {
        self.doctor_with(json, false)
    }

    fn doctor_with(&self, json: bool, diagnostics_to_stderr: bool) -> Result<()> {
        let docker = docker_output(["version", "--format", "{{.Server.Version}}"]);
        let result = serde_json::json!({
            "docker": docker.as_ref().ok(),
            "config_dir": self.root,
            "ok": docker.is_ok(),
        });
        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        if let Ok(version) = docker {
            if !json {
                print_progress(
                    diagnostics_to_stderr,
                    format_args!("✓ Docker {version}\n✓ Config: {}", self.root.display()),
                );
            }
        } else {
            bail!("Docker is not reachable; start Docker Desktop or the Docker daemon");
        }
        Ok(())
    }
}

fn print_progress(diagnostics_to_stderr: bool, message: std::fmt::Arguments<'_>) {
    if diagnostics_to_stderr {
        eprintln!("{message}");
    } else {
        println!("{message}");
    }
}

#[cfg(test)]
#[path = "../tests/unit/runtime/tests.rs"]
mod tests;
