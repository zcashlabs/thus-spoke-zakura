use std::{
    io,
    process::{Child, ExitStatus},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use super::{
    Cancellation, ChildOps, CommandFailureKind, Deadline, HelperSet, LifecyclePolicy, OutputMode,
    run, run_observed,
};
use crate::test_support::{HelperFixture, HelperMode, short_policy};

#[test]
fn clipping_does_not_restart_the_phase_budget() {
    let phase = Deadline::after(Duration::from_millis(80));
    let first = phase.clipped(Duration::from_secs(2));
    std::thread::sleep(Duration::from_millis(30));
    let second = phase.clipped(Duration::from_secs(2));
    assert_eq!(first.0, phase.0);
    assert_eq!(second.0, phase.0);
    assert!(second.remaining() < Duration::from_millis(70));
}

#[test]
fn production_policy_matches_the_lifecycle_contract() {
    let policy = LifecyclePolicy::default();
    assert_eq!(policy.readiness, Duration::from_secs(120));
    assert_eq!(policy.http_attempt, Duration::from_secs(2));
    assert_eq!(policy.readiness_docker, Duration::from_secs(2));
    assert_eq!(policy.startup_docker, Duration::from_secs(30));
    assert_eq!(policy.initialization, Duration::from_secs(120));
    assert_eq!(policy.cleanup, Duration::from_secs(120));
    assert_eq!(policy.poll, Duration::from_millis(25));
    assert_eq!(policy.termination_reserve, Duration::from_millis(250));
    assert_eq!(policy.machine_output_limit, 1024 * 1024);
    assert_eq!(policy.diagnostic_output_limit, 64 * 1024);
    assert_eq!(policy.http_body_limit, 1024 * 1024);
    assert!(!Cancellation::Ignore.requested());
}

#[test]
fn cancellation_terminates_an_active_helper() {
    let policy = short_policy();
    let mut fixture = HelperFixture::new(HelperMode::Hang);
    let cancel = AtomicBool::new(false);
    let mut helpers = HelperSet::new();
    let started = Instant::now();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            fixture.wait_active(Duration::from_secs(1));
            cancel.store(true, Ordering::SeqCst);
        });
        let requested = || cancel.load(Ordering::SeqCst);
        let failure = run(
            &mut fixture.command(),
            Deadline::after(policy.startup_docker),
            Cancellation::Observe(&requested),
            OutputMode::Capture,
            &policy,
            &mut helpers,
        )
        .unwrap_err();
        assert!(matches!(failure.kind, CommandFailureKind::Cancelled));
        assert!(failure.helper_reaped);
    });
    assert!(helpers.is_empty());
    assert!(started.elapsed() < policy.startup_docker + Duration::from_millis(500));
    fixture.finish();
}

#[test]
fn timeout_kills_and_reaps_within_the_operation_allowance() {
    let policy = short_policy();
    let mut fixture = HelperFixture::new(HelperMode::Hang);
    let mut helpers = HelperSet::new();
    let started = Instant::now();
    let failure = run(
        &mut fixture.command(),
        Deadline::after(policy.startup_docker),
        Cancellation::Ignore,
        OutputMode::Capture,
        &policy,
        &mut helpers,
    )
    .unwrap_err();
    assert!(matches!(failure.kind, CommandFailureKind::TimedOut));
    assert!(failure.helper_reaped);
    assert!(helpers.is_empty());
    assert!(started.elapsed() < policy.startup_docker + Duration::from_millis(500));
    fixture.finish();
}

#[test]
fn captured_output_is_capped_while_both_streams_are_drained() {
    let policy = short_policy();
    let mut fixture = HelperFixture::new(HelperMode::FloodBoth);
    let mut helpers = HelperSet::new();
    let output = run(
        &mut fixture.command(),
        Deadline::after(Duration::from_secs(5)),
        Cancellation::Ignore,
        OutputMode::Capture,
        &policy,
        &mut helpers,
    )
    .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout.len(), policy.machine_output_limit);
    assert_eq!(output.stderr.len(), policy.diagnostic_output_limit);
    assert!(output.stdout_truncated);
    assert!(output.stderr_truncated);
    assert!(helpers.is_empty());
    fixture.finish();
}

#[test]
fn nonzero_exit_keeps_diagnostics_and_reaps_the_helper() {
    let policy = short_policy();
    let mut fixture = HelperFixture::new(HelperMode::Nonzero);
    let mut helpers = HelperSet::new();
    let failure = run(
        &mut fixture.command(),
        Deadline::after(Duration::from_secs(2)),
        Cancellation::Ignore,
        OutputMode::Capture,
        &policy,
        &mut helpers,
    )
    .unwrap_err();
    assert!(matches!(failure.kind, CommandFailureKind::Nonzero));
    assert!(failure.stdout.windows(3).any(|window| window == b"out"));
    assert!(failure.stderr.windows(3).any(|window| window == b"err"));
    assert!(failure.helper_reaped);
    assert!(helpers.is_empty());
    fixture.finish();
}

#[test]
fn null_output_still_reaps_a_finite_helper() {
    let policy = short_policy();
    let mut fixture = HelperFixture::new(HelperMode::Nonzero);
    let mut helpers = HelperSet::new();
    let failure = run(
        &mut fixture.command(),
        Deadline::after(Duration::from_secs(2)),
        Cancellation::Ignore,
        OutputMode::Null,
        &policy,
        &mut helpers,
    )
    .unwrap_err();
    assert!(matches!(failure.kind, CommandFailureKind::Nonzero));
    assert!(failure.stdout.is_empty());
    assert!(failure.helper_reaped);
    assert!(helpers.is_empty());
    fixture.finish();
}

#[test]
fn spawn_failure_does_not_invent_a_helper() {
    let policy = short_policy();
    let mut helpers = HelperSet::new();
    let failure = run(
        &mut std::process::Command::new("/no/such/ths-helper"),
        Deadline::after(Duration::from_secs(1)),
        Cancellation::Ignore,
        OutputMode::Capture,
        &policy,
        &mut helpers,
    )
    .unwrap_err();
    assert!(matches!(failure.kind, CommandFailureKind::Spawn));
    assert!(failure.helper_reaped);
    assert!(helpers.is_empty());
}

#[test]
fn capture_setup_failure_reaps_the_child() {
    let policy = short_policy();
    let mut fixture = HelperFixture::new(HelperMode::Hang);
    let mut helpers = HelperSet::new();
    let failure = run_observed(
        &mut fixture.command(),
        Deadline::after(Duration::from_secs(2)),
        Cancellation::Ignore,
        OutputMode::Capture,
        &policy,
        &mut helpers,
        &RealOps,
        true,
    )
    .unwrap_err();
    assert!(matches!(failure.kind, CommandFailureKind::Capture));
    assert!(failure.helper_reaped);
    assert!(helpers.is_empty());
    fixture.finish();
}

#[test]
fn hold_pipe_reaps_the_direct_child_without_waiting_for_the_grandchild() {
    let policy = short_policy();
    let mut fixture = HelperFixture::new(HelperMode::HoldPipe);
    let mut helpers = HelperSet::new();
    let started = Instant::now();
    let failure = run(
        &mut fixture.command(),
        Deadline::after(policy.startup_docker),
        Cancellation::Ignore,
        OutputMode::Capture,
        &policy,
        &mut helpers,
    )
    .unwrap_err();
    assert!(
        matches!(failure.kind, CommandFailureKind::Capture),
        "{failure}"
    );
    assert!(failure.helper_reaped);
    assert!(helpers.is_empty());
    assert!(
        fixture.grandchild_holding(),
        "the grandchild must still hold the inherited pipes"
    );
    assert!(started.elapsed() < policy.startup_docker + Duration::from_millis(500));
    fixture.finish();
}

struct RealOps;

impl ChildOps for RealOps {
    fn kill(&self, child: &mut Child) -> io::Result<()> {
        child.kill()
    }

    fn try_wait(&self, child: &mut Child) -> io::Result<Option<ExitStatus>> {
        child.try_wait()
    }
}

struct FailWait;

impl ChildOps for FailWait {
    fn kill(&self, child: &mut Child) -> io::Result<()> {
        child.kill()
    }

    fn try_wait(&self, _child: &mut Child) -> io::Result<Option<ExitStatus>> {
        Err(io::Error::other("try_wait failed"))
    }
}

#[test]
fn unverified_reap_retains_the_helper_and_is_not_success() {
    let policy = short_policy();
    let mut fixture = HelperFixture::new(HelperMode::Hang);
    let mut helpers = HelperSet::new();
    let failure = run_observed(
        &mut fixture.command(),
        Deadline::after(policy.startup_docker),
        Cancellation::Ignore,
        OutputMode::Capture,
        &policy,
        &mut helpers,
        &FailWait,
        false,
    )
    .unwrap_err();
    assert!(matches!(failure.kind, CommandFailureKind::Reaping));
    assert!(!failure.helper_reaped);
    assert!(!helpers.is_empty());
    assert!(!helpers.unresolved_context().is_empty());
    let _ = helpers.finish(Deadline::after(Duration::from_secs(2)), policy.poll);
    assert!(helpers.is_empty());
    fixture.finish();
}

#[test]
fn termination_reserve_is_inside_the_allowance() {
    let policy = short_policy();
    let deadline = Deadline::after(policy.termination_reserve);
    let reserve = policy.termination_reserve.min(deadline.remaining());
    let execution = deadline.saturating_sub(reserve);
    assert!(execution.expired() || execution.remaining() <= Duration::from_millis(20));
    assert!(
        deadline.0 <= Deadline::after(policy.termination_reserve).0 + Duration::from_millis(50)
    );
}
