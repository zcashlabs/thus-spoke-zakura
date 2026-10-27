//! One JSON-RPC call against the instance's Zakura endpoint.

use super::*;

const RPC_TIMEOUT: Duration = Duration::from_secs(300);
const STOP_REFUSAL: &str = "refusing to call stop without --yes; it shuts down the Zakura node";

impl Runtime {
    pub fn rpc(
        &self,
        name: &InstanceName,
        method: &str,
        params: &[String],
        raw: bool,
        yes: bool,
    ) -> Result<()> {
        println!(
            "{}",
            self.rpc_with(&DockerCli, name, method, params, raw, yes)?
        );
        Ok(())
    }

    /// Sends the call only after the recovery record's Zakura container is still
    /// that container, is owned by this instance, and still publishes the stored
    /// RPC URL. A same-name replacement is not adopted.
    fn rpc_with(
        &self,
        docker: &impl DockerResourceCommands,
        name: &InstanceName,
        method: &str,
        params: &[String],
        raw: bool,
        yes: bool,
    ) -> Result<String> {
        // Refuse before looking the instance up, so a missing confirmation
        // cannot reach the node.
        confirm_stop(method, yes)?;
        let url = self.read_instance(name)?.endpoints.rpc;
        let target = format!("{}-zakura", prefix(name));
        let journal = recovery::RecoveryJournal::load(self.recovery_path(name), name)?
            .with_context(|| {
                format!(
                    "instance {name} has no recovery record; refusing to call a container that only matches by name"
                )
            })?;
        let recorded_id = recorded_container_id(journal.record(), &target)?;
        let details = docker
            .output(&["container", "inspect", &recorded_id])
            .with_context(|| {
                format!(
                    "recorded Zakura container {target} ({recorded_id}) could not be inspected; refusing to adopt a replacement"
                )
            })?;
        let observed = inspected_resource_identity("container", &target, name, &details)?;
        let recorded = recovery::ResourceRef {
            kind: recovery::ResourceKind::Container,
            name: target.clone(),
            identity: Some(recorded_id.clone()),
        };
        if !recovery::identity_matches(&recorded, &observed) {
            bail!(
                "recorded container {target} ({recorded_id}) does not match {observed}; refusing to adopt a replacement"
            );
        }
        let published = published_rpc_port(&details, &target)?;
        let expected = rpc_url_port(&url)?;
        if published != expected {
            bail!(
                "Zakura container {target} publishes RPC on 127.0.0.1:{published}, not the recorded {url}"
            );
        }
        call_rpc(&url, method, params, raw)
    }
}

fn recorded_container_id(record: &recovery::RecoveryRecord, target: &str) -> Result<String> {
    record
        .resources
        .iter()
        .find(|resource| {
            resource.kind == recovery::ResourceKind::Container && resource.name == target
        })
        .and_then(|resource| resource.identity.clone())
        .filter(|identity| !identity.is_empty())
        .with_context(|| {
            format!(
                "no recorded container identity for {target}; refusing to call a container that only matches by name"
            )
        })
}

/// The inspect body must be the recorded container, and its Zakura RPC port
/// must be the loopback binding this instance published.
fn published_rpc_port(details: &str, target: &str) -> Result<u16> {
    let resources: serde_json::Value = serde_json::from_str(details)
        .with_context(|| format!("decoding Docker container {target}"))?;
    let resource = resources
        .as_array()
        .and_then(|items| items.first())
        .with_context(|| format!("Docker returned no container {target}"))?;
    let docker_name = resource["Name"].as_str().unwrap_or("");
    if docker_name != format!("/{target}") {
        bail!(
            "recorded Zakura container is named {docker_name}, not {target}; refusing to adopt a replacement"
        );
    }
    if resource["State"]["Running"].as_bool() != Some(true) {
        bail!("environment container {target} is not running");
    }
    let bindings = resource["NetworkSettings"]["Ports"]["18232/tcp"]
        .as_array()
        .with_context(|| format!("Zakura container {target} has no published RPC port"))?;
    bindings
        .iter()
        .find_map(|binding| {
            if binding["HostIp"].as_str() != Some("127.0.0.1") {
                return None;
            }
            binding["HostPort"].as_str()?.parse().ok()
        })
        .with_context(|| format!("Zakura container {target} does not publish RPC on 127.0.0.1"))
}

fn rpc_url_port(url: &str) -> Result<u16> {
    let parsed =
        reqwest::Url::parse(url).with_context(|| format!("invalid Zakura RPC url {url}"))?;
    parsed
        .port()
        .with_context(|| format!("Zakura RPC url {url} has no port"))
}

/// `stop` shuts Zakura down. Letter case does not make it a different method.
fn confirm_stop(method: &str, yes: bool) -> Result<()> {
    if method.eq_ignore_ascii_case("stop") && !yes {
        bail!("{STOP_REFUSAL}");
    }
    Ok(())
}

/// Instance metadata names a loopback Zakura port. Do not follow a rewritten
/// URL off this machine.
fn ensure_loopback(url: &str) -> Result<()> {
    let parsed =
        reqwest::Url::parse(url).with_context(|| format!("invalid Zakura RPC url {url}"))?;
    match parsed.host_str() {
        Some("127.0.0.1" | "localhost" | "::1") => Ok(()),
        _ => bail!("refusing non-loopback Zakura RPC url {url}"),
    }
}

/// JSON literals stay JSON. Any other argument is a string, as in `cast rpc`.
fn parse_params(params: &[String]) -> Vec<serde_json::Value> {
    params
        .iter()
        .map(|param| {
            serde_json::from_str(param).unwrap_or_else(|_| serde_json::Value::String(param.clone()))
        })
        .collect()
}

fn call_rpc(url: &str, method: &str, params: &[String], raw: bool) -> Result<String> {
    ensure_loopback(url)?;
    let response = reqwest::blocking::Client::builder()
        .timeout(RPC_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building the Zakura RPC client")?
        .post(url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": parse_params(params),
        }))
        .send()
        .with_context(|| format!("calling Zakura {method}"))?;
    let status = response.status();
    let body = response.bytes().context("reading Zakura response")?;
    let envelope: serde_json::Value = serde_json::from_slice(&body).with_context(|| {
        if status.is_success() {
            "decoding Zakura response".to_owned()
        } else {
            format!("Zakura returned HTTP {status}")
        }
    })?;
    if !status.is_success() && rpc_error_message(&envelope).is_none() {
        bail!("Zakura returned HTTP {status}");
    }
    format_rpc_output(raw, &envelope)
}

fn rpc_error_message(envelope: &serde_json::Value) -> Option<String> {
    let error = envelope.get("error").filter(|error| !error.is_null())?;
    Some(
        error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .filter(|message| !message.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| "Zakura RPC returned an error".to_owned()),
    )
}

fn format_rpc_output(raw: bool, envelope: &serde_json::Value) -> Result<String> {
    if let Some(message) = rpc_error_message(envelope) {
        bail!("{message}");
    }
    if raw {
        return Ok(serde_json::to_string_pretty(envelope)?);
    }
    let result = envelope
        .get("result")
        .context("Zakura RPC response has no result")?;
    match result {
        serde_json::Value::String(text) => Ok(text.clone()),
        other => Ok(serde_json::to_string_pretty(other)?),
    }
}

#[cfg(test)]
fn with_rpc_docker<T>(
    docker: impl DockerResourceCommands + 'static,
    body: impl FnOnce() -> T,
) -> T {
    super::docker::with_scripted_docker(docker, body)
}

#[cfg(test)]
#[path = "../../tests/unit/runtime/tests/rpc.rs"]
mod tests;
