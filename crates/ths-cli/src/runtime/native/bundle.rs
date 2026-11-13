use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

pub(super) struct NativeBundle {
    pub(super) bin_dir: PathBuf,
    pub(super) web_dir: PathBuf,
}

#[derive(Deserialize)]
struct Manifest {
    format: u32,
    ths_version: String,
    target: String,
    zakura_commit: String,
    lightwalletd_commit: String,
}

#[derive(Deserialize)]
struct Pins {
    zakura_commit: String,
    lightwalletd_commit: String,
}

pub(super) fn resolve(runtime_dir: Option<&Path>) -> Result<NativeBundle> {
    let directory = match runtime_dir
        .map(Path::to_path_buf)
        .or_else(|| env::var_os("THS_RUNTIME_DIR").map(PathBuf::from))
    {
        Some(directory) => directory,
        None => env::current_exe()?
            .canonicalize()?
            .parent()
            .context("installed launcher has no parent directory")?
            .join(".ths-runtime")
            .join(env!("CARGO_PKG_VERSION")),
    };
    validate(&directory)
}

fn target() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!("native THS supports Linux and macOS on x86_64 and aarch64"),
    }
}

fn validate(directory: &Path) -> Result<NativeBundle> {
    let manifest: Manifest = serde_json::from_slice(
        &fs::read(directory.join("manifest.json")).with_context(|| format!(
            "native runtime is missing at {}; reinstall with THS_INSTALL_RUNTIME=native or pass --runtime-dir",
            directory.display()
        ))?,
    ).context("invalid native runtime manifest")?;
    let pins: Pins = serde_json::from_str(include_str!("../../../../../native/runtime.json"))?;
    ensure!(
        manifest.format == 1,
        "unsupported native runtime manifest format"
    );
    ensure!(
        manifest.ths_version == env!("CARGO_PKG_VERSION"),
        "native runtime version {} does not match launcher {}",
        manifest.ths_version,
        env!("CARGO_PKG_VERSION")
    );
    ensure!(
        manifest.target == target()?,
        "native runtime target {} does not match this platform {}",
        manifest.target,
        target()?
    );
    ensure!(
        manifest.zakura_commit == pins.zakura_commit
            && manifest.lightwalletd_commit == pins.lightwalletd_commit,
        "native runtime component versions do not match this launcher"
    );
    let directory = directory
        .canonicalize()
        .context("resolving native runtime directory")?;
    let bin_dir = directory.join("bin");
    for name in ["zakurad", "lightwalletd", "ths-server"] {
        let path = bin_dir.join(name);
        let metadata = fs::metadata(&path)
            .with_context(|| format!("missing native executable {}", path.display()))?;
        ensure!(
            metadata.is_file(),
            "native executable is not a file: {}",
            path.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                metadata.permissions().mode() & 0o111 != 0,
                "native binary is not executable: {}",
                path.display()
            );
        }
    }
    let web_dir = directory.join("web");
    ensure!(
        web_dir.join("index.html").is_file(),
        "native runtime is missing web/index.html"
    );
    Ok(NativeBundle { bin_dir, web_dir })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("bin")).unwrap();
        fs::create_dir(directory.path().join("web")).unwrap();
        fs::write(directory.path().join("web/index.html"), "test dashboard").unwrap();
        for name in ["zakurad", "lightwalletd", "ths-server"] {
            let path = directory.path().join("bin").join(name);
            fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let pins: Pins =
            serde_json::from_str(include_str!("../../../../../native/runtime.json")).unwrap();
        let manifest = serde_json::json!({"format":1, "ths_version":env!("CARGO_PKG_VERSION"),
            "target":target().unwrap(), "zakura_commit":pins.zakura_commit,
            "lightwalletd_commit":pins.lightwalletd_commit});
        fs::write(directory.path().join("manifest.json"), manifest.to_string()).unwrap();
        directory
    }

    #[test]
    fn requires_matching_complete_bundle() {
        let fixture = fixture();
        let bundle = validate(fixture.path()).unwrap();
        assert!(bundle.bin_dir.join("zakurad").is_file());
        assert!(bundle.web_dir.join("index.html").is_file());
        for (field, value) in [
            ("format", serde_json::json!(2)),
            ("ths_version", serde_json::json!("0.0.0")),
            ("target", serde_json::json!("wrong-platform")),
            ("zakura_commit", serde_json::json!("wrong")),
            ("lightwalletd_commit", serde_json::json!("wrong")),
        ] {
            let path = fixture.path().join("manifest.json");
            let original = fs::read(&path).unwrap();
            let mut manifest: serde_json::Value = serde_json::from_slice(&original).unwrap();
            manifest[field] = value;
            fs::write(&path, manifest.to_string()).unwrap();
            assert!(validate(fixture.path()).is_err(), "accepted bad {field}");
            fs::write(path, original).unwrap();
        }
        fs::remove_file(bundle.bin_dir.join("lightwalletd")).unwrap();
        assert!(validate(fixture.path()).is_err());
    }

    #[test]
    fn rejects_non_executable_and_missing_dashboard() {
        let fixture = fixture();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = fixture.path().join("bin/zakurad");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(validate(fixture.path()).is_err());
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::remove_file(fixture.path().join("web/index.html")).unwrap();
        assert!(validate(fixture.path()).is_err());
    }
}
