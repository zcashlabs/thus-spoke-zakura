//! Bounded health and Zakura tip probes.
//!
//! Each attempt uses the caller's phase deadline clipped to the HTTP attempt cap.
//! A retry sleeps on the time left in that same deadline and does not open a new one.

use std::{io::Read, time::Duration};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;

use crate::lifecycle::{Cancellation, Deadline, LifecyclePolicy};

use super::{DockerResourceCommands, Shutdown};

const RPC_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"getbestblockhash","params":[]}"#;

#[derive(Clone, Copy, Debug)]
pub(super) enum Probe {
    Health,
    ZakuraTip,
}

pub(super) fn readiness_client() -> Result<Client> {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building the readiness client")
}

pub(super) fn probe_once(
    client: &Client,
    base: &str,
    probe: Probe,
    deadline: Deadline,
    cancellation: Cancellation<'_>,
    policy: &LifecyclePolicy,
) -> Result<bool> {
    ensure_loopback(base)?;
    if cancellation.requested() {
        bail!("cancelled readiness probe for {base}");
    }
    if deadline.expired() {
        bail!("timed out before the readiness probe for {base}");
    }
    let attempt = deadline.clipped(policy.http_attempt);
    let timeout = attempt.remaining();
    if timeout.is_zero() {
        bail!("timed out before the readiness probe for {base}");
    }
    let url = match probe {
        Probe::Health => format!("{base}/api/v1/health"),
        Probe::ZakuraTip => base.to_owned(),
    };
    let response = match probe {
        Probe::Health => client.get(&url).timeout(timeout).send(),
        Probe::ZakuraTip => client
            .post(&url)
            .timeout(timeout)
            .header("content-type", "application/json")
            .body(RPC_BODY)
            .send(),
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            if error.is_timeout() || attempt.expired() || deadline.expired() {
                bail!("timed out waiting for {url}: {error}");
            }
            return Ok(false);
        }
    };
    let status = response.status().as_u16();
    let body = read_body(response, policy.http_body_limit)?;
    if cancellation.requested() {
        bail!("cancelled readiness probe for {base}");
    }
    if deadline.expired() || attempt.expired() {
        bail!("timed out after a late readiness response from {url}");
    }
    match probe {
        Probe::Health => Ok(status < 400),
        Probe::ZakuraTip => {
            if status >= 400 {
                return Ok(false);
            }
            let value: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(value) => value,
                Err(_) => return Ok(false),
            };
            Ok(value
                .get("result")
                .and_then(|result| result.as_str())
                .is_some())
        }
    }
}

fn read_body(response: reqwest::blocking::Response, limit: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    response
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut body)
        .context("reading a readiness response body")?;
    if body.len() > limit {
        bail!("oversized readiness response body");
    }
    Ok(body)
}

fn ensure_loopback(base: &str) -> Result<()> {
    let url = reqwest::Url::parse(base).with_context(|| format!("invalid probe url {base}"))?;
    match url.host_str() {
        Some("127.0.0.1" | "localhost" | "::1") => Ok(()),
        _ => bail!("refusing non-loopback readiness probe {base}"),
    }
}

pub(super) fn inspect_running(docker: &impl DockerResourceCommands, id: &str) -> Result<bool> {
    let output = docker.output(&["container", "inspect", "--format", "{{.State.Running}}", id])?;
    match output.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => bail!("malformed container state for {id}: {other}"),
    }
}

pub(super) fn diagnostic_logs(docker: &impl DockerResourceCommands, id: &str) -> Result<String> {
    docker
        .output(&["logs", "--tail", "50", id])
        .with_context(|| format!("reading logs for {id}"))
}

pub(super) fn wait_ready(
    base: &str,
    container_id: &str,
    deadline: Deadline,
    shutdown: &Shutdown,
    policy: &LifecyclePolicy,
    docker: &impl DockerResourceCommands,
) -> Result<()> {
    wait_for_probe(
        base,
        container_id,
        Probe::Health,
        "dashboard did not become healthy",
        "app exited before becoming healthy",
        Duration::from_millis(750),
        deadline,
        shutdown,
        policy,
        docker,
    )
}

pub(super) fn wait_for_zakura_tip(
    base: &str,
    container_id: &str,
    deadline: Deadline,
    shutdown: &Shutdown,
    policy: &LifecyclePolicy,
    docker: &impl DockerResourceCommands,
) -> Result<()> {
    wait_for_probe(
        base,
        container_id,
        Probe::ZakuraTip,
        "Zakura RPC tip did not become available",
        "Zakura exited before its RPC tip became available",
        Duration::from_millis(250),
        deadline,
        shutdown,
        policy,
        docker,
    )
}

#[allow(clippy::too_many_arguments)]
fn wait_for_probe(
    base: &str,
    container_id: &str,
    probe: Probe,
    timeout_message: &str,
    stopped_message: &str,
    retry_delay: Duration,
    deadline: Deadline,
    shutdown: &Shutdown,
    policy: &LifecyclePolicy,
    docker: &impl DockerResourceCommands,
) -> Result<()> {
    let client = readiness_client()?;
    let mut primary = "readiness probe did not succeed".to_owned();
    loop {
        if shutdown.try_interrupted() {
            bail!("interrupted");
        }
        if deadline.expired() {
            bail!("timed out: {timeout_message}; {primary}");
        }
        let requested = || shutdown.try_interrupted();
        match probe_once(
            &client,
            base,
            probe,
            deadline,
            Cancellation::Observe(&requested),
            policy,
        ) {
            Ok(true) => {
                if shutdown.try_interrupted() {
                    bail!("interrupted");
                }
                if deadline.expired() {
                    bail!("timed out: {timeout_message}; {primary}");
                }
                return Ok(());
            }
            Ok(false) => primary = format!("{probe:?} probe was not ready"),
            Err(error) => primary = format!("{error:#}"),
        }
        if deadline.expired() {
            bail!("timed out: {timeout_message}; {primary}");
        }
        match inspect_running(docker, container_id) {
            Ok(true) => {}
            Ok(false) => match diagnostic_logs(docker, container_id) {
                Ok(logs) => bail!("{primary}; {stopped_message}:\n{logs}"),
                Err(error) => {
                    bail!("{primary}; {stopped_message}; diagnostic logs failed: {error:#}")
                }
            },
            Err(error) => {
                bail!("{primary}; Docker inspection failed: {error:#}");
            }
        }
        let delay = retry_delay.min(deadline.remaining());
        if delay.is_zero() {
            bail!("timed out: {timeout_message}; {primary}");
        }
        shutdown.wait_timeout(delay)?;
    }
}
