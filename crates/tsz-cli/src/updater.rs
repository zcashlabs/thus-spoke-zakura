use std::{
    env, fs,
    io::Write,
    path::Path,
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

    let executable = env::current_exe().context("locating the running launcher")?;
    let install_dir = executable
        .parent()
        .context("the running launcher has no parent directory")?;
    run_installer(INSTALLER, &target, install_dir)?;
    Ok(ExitCode::SUCCESS)
}

pub fn uninstall() -> Result<ExitCode> {
    ensure_release_distribution()?;
    let executable = env::current_exe().context("locating the running launcher")?;
    remove_installed_executable(&executable)?;
    println!("Uninstalled ths from {}.", executable.display());
    println!("Cached Docker images and configuration were left in place.");
    Ok(ExitCode::SUCCESS)
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
    let endpoint = env::var("TSZ_RELEASE_API_URL").unwrap_or_else(|_| RELEASE_API.to_owned());
    latest_version_from(&endpoint)
}

fn latest_version_from(endpoint: &str) -> Result<Version> {
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
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

fn run_installer(script: &str, version: &Version, install_dir: &Path) -> Result<()> {
    let mut child = Command::new("sh")
        .arg("-s")
        .env("TSZ_VERSION", version.to_string())
        .env("TSZ_INSTALL_DIR", install_dir)
        .stdin(Stdio::piped())
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
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    use super::*;

    #[test]
    fn parses_prefixed_versions_and_compares_all_states() {
        let current = parse_version("v1.2.3").unwrap();
        assert_eq!(
            compare_versions(&current, &parse_version("1.2.4").unwrap()),
            Status::UpdateAvailable
        );
        assert_eq!(
            compare_versions(&current, &parse_version("1.2.3").unwrap()),
            Status::UpToDate
        );
        assert_eq!(
            compare_versions(&current, &parse_version("1.2.2").unwrap()),
            Status::Ahead
        );
        assert!(parse_version("latest").is_err());
        assert!(parse_version("1.2.3-beta.1").is_err());
    }

    #[test]
    fn check_uses_a_distinct_update_available_exit_code() {
        assert_eq!(check_exit_code(Status::UpToDate), ExitCode::SUCCESS);
        assert_eq!(check_exit_code(Status::Ahead), ExitCode::SUCCESS);
        assert_eq!(check_exit_code(Status::UpdateAvailable), ExitCode::from(10));
    }

    #[test]
    fn discovers_latest_release_and_rejects_http_errors() {
        let endpoint = serve_once("200 OK", r#"{"tag_name":"v2.3.4"}"#);
        assert_eq!(
            latest_version_from(&endpoint).unwrap(),
            Version::new(2, 3, 4)
        );

        let endpoint = serve_once("403 Forbidden", r#"{"message":"rate limited"}"#);
        let error = latest_version_from(&endpoint).unwrap_err().to_string();
        assert!(error.contains("HTTP 403"));
    }

    #[test]
    fn embedded_installer_receives_exact_version_and_destination() {
        let version = Version::new(4, 5, 6);
        run_installer(
            "test \"$TSZ_VERSION\" = 4.5.6 && test \"$TSZ_INSTALL_DIR\" = /tmp/tsz\n",
            &version,
            Path::new("/tmp/tsz"),
        )
        .unwrap();
        assert!(run_installer("exit 7\n", &version, Path::new("/tmp/tsz")).is_err());
    }

    #[test]
    fn source_builds_cannot_mutate_themselves() {
        if !cfg!(feature = "release-distribution") {
            assert!(ensure_release_distribution().is_err());
        }
    }

    #[test]
    fn uninstall_removes_only_an_executable_named_ths() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("ths");
        std::fs::write(&executable, "launcher").unwrap();

        remove_installed_executable(&executable).unwrap();
        assert!(!executable.exists());

        let unexpected = directory.path().join("another-tool");
        std::fs::write(&unexpected, "keep me").unwrap();
        assert!(remove_installed_executable(&unexpected).is_err());
        assert!(unexpected.exists());
    }

    fn serve_once(status: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request);
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        format!("http://{address}")
    }
}
