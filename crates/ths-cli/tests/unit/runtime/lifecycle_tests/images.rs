use super::*;

#[test]
fn missing_image_advice_requires_a_completed_listing() {
    let policy = short_policy();
    let context = LifecycleContext::new(policy);
    let script = image_listing_script(false);
    let docker = script_docker(&script, &context, policy.startup_docker);
    let error = startup_require_image(&docker, "example/image:missing").unwrap_err();
    assert!(error.to_string().contains("ths pull"), "{error:#}");

    let present = image_listing_script(true);
    let docker = script_docker(&present, &context, policy.startup_docker);
    startup_require_image(&docker, "example/image:missing")
        .expect("a completed listing that contains the image is not a pull failure");

    let sleep = sleep_script();
    let docker = LifecycleDocker {
        context: &context,
        enclosing_deadline: None,
        command_cap: policy.startup_docker,
        cancellation: Cancellation::Ignore,
        program: "/bin/sh".into(),
        prefix_args: vec![sleep.path().join("docker").to_string_lossy().into_owned()],
        env: Vec::new(),
        child_ops: None,
    };
    let started = Instant::now();
    let error = startup_require_image(&docker, "example/image:missing").unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("timed out"), "{text}");
    assert!(!text.contains("ths pull"), "{text}");
    assert!(started.elapsed() < policy.startup_docker + Duration::from_millis(500));
    let _ = context.finish_helpers(Deadline::after(policy.startup_docker));
}
