use super::*;
use crate::runtime::watch::{EnvironmentState, Tick};
use std::collections::VecDeque;

fn entry(txid: &str, status: &str) -> serde_json::Value {
    serde_json::json!({
        "id": format!("wallet:send:{txid}"),
        "kind": "transaction",
        "direction": "outgoing",
        "from_account": 1,
        "to_account": 2,
        "to_address": null,
        "source_pool": "ironwood",
        "destination_pool": "ironwood",
        "source_pools": ["ironwood"],
        "destination_pools": ["ironwood"],
        "amount_zatoshi": 50_000_000,
        "txid": txid,
        "block_hash": "block",
        "mined_height": 100,
        "status": status,
        "created_at": null,
        "last_error": null,
    })
}

fn ticks(count: usize) -> impl Fn(Duration) -> Result<Tick> {
    let remaining = RefCell::new(count);
    move |_| {
        let mut remaining = remaining.borrow_mut();
        if *remaining > 0 {
            *remaining -= 1;
            Ok(Tick::Poll)
        } else {
            Ok(Tick::Ended(Ended::Interrupted))
        }
    }
}

fn probes(states: Vec<EnvironmentState>) -> impl Fn() -> EnvironmentState {
    let states = RefCell::new(VecDeque::from(states));
    move || {
        states
            .borrow_mut()
            .pop_front()
            .unwrap_or(EnvironmentState::Running)
    }
}

fn watch(
    responses: Vec<(&'static str, String)>,
    tty: bool,
    json: bool,
    probe_states: Vec<EnvironmentState>,
    tick: impl Fn(Duration) -> Result<Tick>,
) -> (Result<()>, String, String) {
    watch_limited(responses, tty, json, None, probe_states, tick)
}

fn watch_limited(
    responses: Vec<(&'static str, String)>,
    tty: bool,
    json: bool,
    limit: Option<u32>,
    probe_states: Vec<EnvironmentState>,
    tick: impl Fn(Duration) -> Result<Tick>,
) -> (Result<()>, String, String) {
    let (url, _requests) = serve(responses);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = runtime_for_tests().wallet_activity_watch_with(
        &name("alpha"),
        &url,
        &reqwest::blocking::Client::new(),
        &mut out,
        &mut err,
        tty,
        limit,
        json,
        &probes(probe_states),
        &tick,
    );
    (
        result,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn watch_emits_piped_snapshots_only_on_change() {
    let (result, output, _err) = watch(
        vec![
            ("200 OK", "[]".to_owned()),
            ("200 OK", "[]".to_owned()),
            ("200 OK", format!("[{}]", entry("first", "confirmed"))),
            ("200 OK", format!("[{}]", entry("second", "confirmed"))),
        ],
        false,
        false,
        vec![],
        ticks(3),
    );

    result.unwrap();
    // Four polls, three emits: the identical second snapshot is skipped.
    assert_eq!(output.matches("Watching alpha ·").count(), 3, "{output}");
    assert!(output.contains("send  confirmed  0.5 ZEC"), "{output}");
}

#[test]
fn watch_redraws_every_poll_on_tty() {
    let (result, output, _err) = watch(
        vec![("200 OK", "[]".to_owned()), ("200 OK", "[]".to_owned())],
        true,
        false,
        vec![],
        ticks(1),
    );

    result.unwrap();
    assert_eq!(output.matches("\x1b[2J\x1b[H").count(), 2, "{output}");
}

#[test]
fn watch_marks_stopped_and_recovers() {
    let (result, output, _err) = watch(
        vec![
            ("200 OK", "[]".to_owned()),
            ("500 Internal Server Error", String::new()),
            ("200 OK", "[]".to_owned()),
        ],
        false,
        false,
        vec![EnvironmentState::Stopped],
        ticks(2),
    );

    result.unwrap();
    assert!(output.contains("environment alpha stopped"), "{output}");
    // Recovery prints a fresh frame rather than staying stale.
    assert_eq!(output.matches("Watching alpha ·").count(), 3, "{output}");
}

#[test]
fn watch_reports_unreachable_reads_and_keeps_polling() {
    let (result, output, _err) = watch(
        vec![
            ("200 OK", "[]".to_owned()),
            ("500 Internal Server Error", String::new()),
            ("200 OK", "[]".to_owned()),
        ],
        false,
        false,
        vec![EnvironmentState::Running],
        ticks(2),
    );

    result.unwrap();
    assert!(output.contains("unreachable ("), "{output}");
    assert_eq!(output.matches("Watching alpha ·").count(), 3, "{output}");
}

#[test]
fn watch_json_emits_ndjson_snapshots_only_on_change() {
    let (result, output, _err) = watch(
        vec![
            ("200 OK", "[]".to_owned()),
            ("200 OK", "[]".to_owned()),
            ("200 OK", format!("[{}]", entry("first", "confirmed"))),
        ],
        false,
        true,
        vec![],
        ticks(2),
    );

    result.unwrap();
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines.len(), 2, "{output}");
    for line in &lines {
        assert!(
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap()
                .is_array()
        );
    }
    assert_eq!(lines[0], "[]");
    assert!(lines[1].contains("\"txid\":\"first\""));
}

#[test]
fn watch_exits_when_the_environment_was_deleted() {
    let (result, output, _err) = watch(
        vec![("500 Internal Server Error", String::new())],
        false,
        false,
        vec![EnvironmentState::Deleted],
        ticks(0),
    );
    assert_eq!(
        result.unwrap_err().to_string(),
        "environment alpha was deleted"
    );
    assert!(output.is_empty(), "{output}");
}

#[test]
fn watch_exits_when_the_monitor_reports_deletion() {
    let (result, _output, _err) = watch(
        vec![("200 OK", "[]".to_owned())],
        false,
        false,
        vec![],
        |_| Ok(Tick::Ended(Ended::DeletedElsewhere)),
    );
    assert_eq!(
        result.unwrap_err().to_string(),
        "environment alpha was deleted"
    );
}

#[test]
fn watch_forwards_the_limit_to_every_poll() {
    let (url, requests) = serve(vec![
        ("200 OK", "[]".to_owned()),
        ("200 OK", "[]".to_owned()),
    ]);
    let mut out = Vec::new();
    let mut err = Vec::new();
    runtime_for_tests()
        .wallet_activity_watch_with(
            &name("alpha"),
            &url,
            &reqwest::blocking::Client::new(),
            &mut out,
            &mut err,
            false,
            Some(5),
            false,
            &probes(vec![]),
            &ticks(1),
        )
        .unwrap();
    for _ in 0..2 {
        let (request_line, _) = requests.recv().unwrap();
        assert_eq!(request_line, "GET /api/v1/activity?limit=5 HTTP/1.1");
    }
}

#[test]
fn watch_interrupted_exits_cleanly() {
    let (result, _output, _err) = watch(
        vec![("200 OK", "[]".to_owned())],
        false,
        false,
        vec![],
        |_| Ok(Tick::Ended(Ended::Interrupted)),
    );
    result.unwrap();
}

#[test]
fn watch_bounds_a_stalled_read_by_the_request_timeout() {
    let (url, _requests) = serve(vec![("hang", String::new())]);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(50))
        .build()
        .unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();

    let started = Instant::now();
    runtime_for_tests()
        .wallet_activity_watch_with(
            &name("alpha"),
            &url,
            &client,
            &mut out,
            &mut err,
            false,
            None,
            false,
            &probes(vec![]),
            &ticks(2),
        )
        .unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "stalled reads must time out instead of blocking the loop: {:?}",
        started.elapsed()
    );
    let output = String::from_utf8(out).unwrap();
    assert!(output.contains("unreachable ("), "{output}");
}

#[test]
fn watch_json_warns_again_after_a_recovered_outage() {
    let (result, _output, err) = watch(
        vec![
            ("200 OK", "[]".to_owned()),
            ("500 Internal Server Error", String::new()),
            ("200 OK", "[]".to_owned()),
            ("500 Internal Server Error", String::new()),
        ],
        false,
        true,
        vec![EnvironmentState::Stopped, EnvironmentState::Stopped],
        ticks(3),
    );

    result.unwrap();
    // The warning state resets on a successful read, so a second outage with
    // the same error warns again instead of staying silent.
    assert_eq!(err.matches("environment alpha stopped").count(), 2, "{err}");
}
