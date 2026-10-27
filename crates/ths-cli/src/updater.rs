use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::{Deserialize, Serialize};

const RELEASE_API: &str =
    "https://api.github.com/repos/zcashlabs/thus-spoke-zakura/releases/latest";
const INSTALLER: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../install.sh"));
const UPDATE_AVAILABLE_EXIT: u8 = 10;
const STARTUP_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    UpToDate,
    UpdateAvailable,
    Ahead,
}

#[derive(Debug, Serialize)]
struct Report<'a> {
    current: &'a Version,
    available: &'a Version,
    status: Status,
}

pub fn run(requested: Option<&str>, check: bool, json: bool) -> Result<ExitCode> {
    let current = parse_version(env!("CARGO_PKG_VERSION"))?;

    if check {
        let available = latest_version()?;
        let status = compare_versions(&current, &available);
        print_report(&current, &available, status, json)?;
        return Ok(check_exit_code(status));
    }

    ensure_release_distribution()?;
    let target = match requested {
        Some(version) => parse_version(version)?,
        None => {
            let available = latest_version()?;
            let status = compare_versions(&current, &available);
            if status != Status::UpdateAvailable {
                print_report(&current, &available, status, json)?;
                return Ok(ExitCode::SUCCESS);
            }
            available
        }
    };

    let launcher = installed_launcher()?;
    let install_dir = launcher
        .parent()
        .context("the running launcher has no parent directory")?;
    run_installer(INSTALLER, &target, install_dir, json)?;
    if json {
        print_report(&target, &target, Status::UpToDate, true)?;
    }
    Ok(ExitCode::SUCCESS)
}

pub fn startup_notice() -> Option<String> {
    if !cfg!(feature = "release-distribution") {
        return None;
    }

    let current = parse_version(env!("CARGO_PKG_VERSION")).ok()?;
    let endpoint = env::var("THS_RELEASE_API_URL").unwrap_or_else(|_| RELEASE_API.to_owned());
    startup_notice_from(&endpoint, &current)
}

fn startup_notice_from(endpoint: &str, current: &Version) -> Option<String> {
    let available = latest_version_from_with_timeout(endpoint, STARTUP_CHECK_TIMEOUT).ok()?;
    (compare_versions(current, &available) == Status::UpdateAvailable).then(|| {
        format!("Update available: {current} → {available}. Run `ths update` to install it.")
    })
}

pub fn uninstall() -> Result<ExitCode> {
    ensure_release_distribution()?;
    let launcher = installed_launcher()?;
    remove_installed_executable(&launcher)?;
    println!("Uninstalled ths from {}.", launcher.display());
    println!("Cached Docker images and configuration were left in place.");
    Ok(ExitCode::SUCCESS)
}

/// The launcher file that update and uninstall act on. On macOS `current_exe` returns the path the
/// launcher was run through, so a symlinked `ths` would be replaced or removed instead of the real
/// launcher. Resolving it leaves the symlink in place.
fn installed_launcher() -> Result<PathBuf> {
    resolve_launcher(&env::current_exe().context("locating the running launcher")?)
}

fn resolve_launcher(executable: &Path) -> Result<PathBuf> {
    fs::canonicalize(executable)
        .with_context(|| format!("resolving the running launcher {}", executable.display()))
}

fn remove_installed_executable(executable: &Path) -> Result<()> {
    if executable.file_name().and_then(|name| name.to_str()) != Some("ths") {
        bail!(
            "refusing to remove unexpected executable {}; uninstall ths from its installed path",
            executable.display()
        );
    }
    fs::remove_file(executable)
        .with_context(|| format!("removing installed launcher {}", executable.display()))
}

fn ensure_release_distribution() -> Result<()> {
    if !cfg!(feature = "release-distribution") {
        bail!(
            "self-update is available only in an official release binary; update this source build with Git/Cargo, or rerun the public installer"
        );
    }
    Ok(())
}

fn latest_version() -> Result<Version> {
    let endpoint = env::var("THS_RELEASE_API_URL").unwrap_or_else(|_| RELEASE_API.to_owned());
    latest_version_from(&endpoint)
}

fn latest_version_from(endpoint: &str) -> Result<Version> {
    latest_version_from_with_timeout(endpoint, Duration::from_secs(15))
}

fn latest_version_from_with_timeout(endpoint: &str, timeout: Duration) -> Result<Version> {
    let response = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .context("creating the release client")?
        .get(endpoint)
        .header(reqwest::header::USER_AGENT, "ths-updater")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .with_context(|| format!("checking the latest release at {endpoint}"))?;
    if !response.status().is_success() {
        bail!(
            "release lookup failed with HTTP {}; check your network access and GitHub permissions",
            response.status()
        );
    }
    let release: Release = response.json().context("decoding the latest release")?;
    parse_version(&release.tag_name).context("the latest release tag is not semantic")
}

fn parse_version(value: &str) -> Result<Version> {
    let version = Version::parse(value.strip_prefix('v').unwrap_or(value))
        .with_context(|| format!("invalid semantic version {value:?}"))?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        bail!("release versions must use stable X.Y.Z form, got {value:?}");
    }
    Ok(version)
}

fn compare_versions(current: &Version, available: &Version) -> Status {
    use std::cmp::Ordering;
    match current.cmp(available) {
        Ordering::Less => Status::UpdateAvailable,
        Ordering::Equal => Status::UpToDate,
        Ordering::Greater => Status::Ahead,
    }
}

fn check_exit_code(status: Status) -> ExitCode {
    if status == Status::UpdateAvailable {
        ExitCode::from(UPDATE_AVAILABLE_EXIT)
    } else {
        ExitCode::SUCCESS
    }
}

fn print_report(current: &Version, available: &Version, status: Status, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&Report {
                current,
                available,
                status,
            })?
        );
    } else {
        match status {
            Status::UpToDate => println!("ths {current} is up to date."),
            Status::UpdateAvailable => {
                println!("Update available: {current} → {available}");
            }
            Status::Ahead => {
                println!("ths {current} is newer than the latest release ({available}).")
            }
        }
    }
    Ok(())
}

fn run_installer(script: &str, version: &Version, install_dir: &Path, json: bool) -> Result<()> {
    let mut child = Command::new("sh")
        .arg("-s")
        .env("THS_VERSION", version.to_string())
        .env("THS_INSTALL_DIR", install_dir)
        .stdin(Stdio::piped())
        .stdout(if json {
            Stdio::from(std::io::stderr())
        } else {
            Stdio::inherit()
        })
        .spawn()
        .context("starting the embedded installer")?;
    child
        .stdin
        .take()
        .context("opening the embedded installer input")?
        .write_all(script.as_bytes())
        .context("passing the embedded installer to the shell")?;
    let status = child.wait().context("waiting for the embedded installer")?;
    if !status.success() {
        bail!("the verified installer failed; the current launcher was not replaced");
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/updater/tests.rs"]
mod tests;
