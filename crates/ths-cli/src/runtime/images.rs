use super::*;

pub(super) const APP_IMAGE_REPOSITORY: &str = "ghcr.io/zcashlabs/thus-spoke-zakura-app";
pub(super) const ZAKURA_IMAGE: &str = "zakuracore/zakura:1.7.0";
pub(super) const LIGHTWALLETD_IMAGE_REPOSITORY: &str =
    "ghcr.io/zcashlabs/thus-spoke-zakura-lightwalletd";
pub(super) fn app_image() -> String {
    format!("{APP_IMAGE_REPOSITORY}:{}", env!("CARGO_PKG_VERSION"))
}

pub(super) fn lightwalletd_image() -> String {
    format!(
        "{LIGHTWALLETD_IMAGE_REPOSITORY}:{}",
        env!("CARGO_PKG_VERSION")
    )
}

impl Runtime {
    pub fn build(&self, dev: bool) -> Result<()> {
        self.doctor(false)?;
        build_project_images(dev)?;
        ensure_image(ZAKURA_IMAGE)?;
        println!("Runtime images are ready.");
        Ok(())
    }

    pub fn pull(&self) -> Result<()> {
        self.doctor(false)?;
        for image in [app_image(), lightwalletd_image(), ZAKURA_IMAGE.to_owned()] {
            println!("Pulling {image}…");
            docker(["pull", &image])?;
        }
        println!("Runtime images are ready.");
        Ok(())
    }
}

/// Bounds a pull only to end a hung download; a slow first download must still finish.
const IMAGE_PULL_CAP: Duration = Duration::from_secs(30 * 60);

/// Pulls this launcher's images that are not present, so `--docker` works after an install that
/// fetched only the native runtime. Callers check that Docker is reachable first and hold the
/// instance, so `stop` and Ctrl+C cancel a pull. Docker's progress goes to stderr because
/// `--json start` and the `run` child own stdout. Source builds never pull: a published image
/// with their version tag would not contain local changes.
pub(super) fn pull_missing_images(docker: &LifecycleDocker<'_>) -> Result<()> {
    if !cfg!(feature = "release-distribution") {
        return Ok(());
    }
    let pull = docker.scoped(Deadline::after(IMAGE_PULL_CAP), IMAGE_PULL_CAP);
    for image in [app_image(), lightwalletd_image(), ZAKURA_IMAGE.to_owned()] {
        // Any other inspection failure is left to `startup_require_image`'s diagnosis.
        if docker
            .execute(
                &["image", "inspect", "--format", "{{.Id}}", &image],
                OutputMode::Capture,
            )
            .is_ok()
        {
            continue;
        }
        eprintln!("Pulling {image}…");
        pull.execute(&["pull", &image], OutputMode::InheritStderr)
            .map_err(|error| anyhow!("{error}").context(format!("pulling {image}")))?;
    }
    Ok(())
}

pub(super) fn ensure_image(image: &str) -> Result<()> {
    if docker_output(["image", "inspect", image]).is_err() {
        println!("Pulling {image}…");
        docker(["pull", image])?;
    }
    Ok(())
}
pub(super) fn build_project_images(dev: bool) -> Result<()> {
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !project_root.join("Dockerfile").is_file()
        || !project_root
            .join("docker/lightwalletd.Dockerfile")
            .is_file()
    {
        bail!(
            "cannot build images: project source is unavailable at {}",
            project_root.display()
        );
    }

    let app_image = app_image();
    let lightwalletd_image = lightwalletd_image();
    if dev {
        println!("Building {app_image} with the Rust development profile…");
        docker_inherit_in(
            &[
                "build",
                "--build-arg",
                "RUST_PROFILE=dev-runtime",
                "-t",
                &app_image,
                ".",
            ],
            &project_root,
        )?;
    } else {
        println!("Building {app_image}…");
        docker_inherit_in(&["build", "-t", &app_image, "."], &project_root)?;
    }
    println!("Building {lightwalletd_image}…");
    docker_inherit_in(
        &[
            "build",
            "-f",
            "docker/lightwalletd.Dockerfile",
            "-t",
            &lightwalletd_image,
            ".",
        ],
        &project_root,
    )
}
