use super::*;

#[test]
fn chain_changes_forward_the_command_and_report_retention_failures_in_both_outputs() {
    let response = serde_json::json!({
        "depth":3,"old_tip":{"height":108,"hash":"old-tip"},
        "new_tip":{"height":108,"hash":"new-tip"},
        "removed_hashes":["a","b","c"],"replacement_hashes":["d","e","f"],
        "retained_txids":["kept"],"failed_transactions":[{"txid":"dropped","error":"invalid anchor"}]
    });
    for (operation, keep_txs, body) in [
        (
            "reorg",
            true,
            serde_json::json!({"depth":3,"keep_txs":true}),
        ),
        (
            "reorg",
            false,
            serde_json::json!({"depth":3,"keep_txs":false}),
        ),
        ("rollback", false, serde_json::json!({"depth":3})),
    ] {
        let (url, requests) = serve(vec![("200 OK", response.to_string())]);
        let result = change_chain(
            &url,
            &reqwest::blocking::Client::new(),
            operation,
            3,
            keep_txs,
        )
        .unwrap();
        let (request_line, request_body) = requests.recv().unwrap();
        assert_eq!(request_line, format!("POST /api/v1/{operation} HTTP/1.1"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&request_body).unwrap(),
            body
        );
        let json = render_chain_change(&result, &name("alpha"), operation, true).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            response
        );
        let text = render_chain_change(&result, &name("alpha"), operation, false).unwrap();
        assert!(text.contains("3 blocks on alpha"));
        assert!(text.contains("New tip: 108 (new-tip)"));
        assert!(text.contains("Not retained: dropped: invalid anchor"));
        assert!(text.starts_with(if operation == "reorg" {
            "Reorganized"
        } else {
            "Rolled back"
        }));
    }
}

#[test]
fn chain_changes_do_not_retry_rejected_or_lost_responses() {
    for status in ["409 Conflict", "drop"] {
        let (url, requests) = serve(vec![(status, "busy")]);
        let error =
            change_chain(&url, &reqwest::blocking::Client::new(), "reorg", 3, false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("inspect the current tip before retrying")
        );
        assert_eq!(requests.recv().unwrap().0, "POST /api/v1/reorg HTTP/1.1");
        assert!(requests.try_recv().is_err());
    }
}

#[test]
fn responses_decode_or_report_their_status_and_body() {
    let (url, _requests) = serve(vec![
        (
            "200 OK",
            r#"{"id":"job-1","requested_blocks":1,"completed_blocks":1,"state":"completed","error":null,"progress_uncertain":false,"hashes":["tip"]}"#,
        ),
        ("409 Conflict", "already claimed"),
        ("200 OK", "not json"),
    ]);
    let client = reqwest::blocking::Client::new();
    let get = || client.get(&url).send().unwrap();

    let job: MiningJob = decode_response(get()).unwrap();
    assert_eq!(job.state, MiningJobState::Completed);
    let rejected = decode_response::<MiningJob>(get()).unwrap_err().to_string();
    assert_eq!(rejected, "rejected (409 Conflict): already claimed");
    let malformed = format!("{:#}", decode_response::<MiningJob>(get()).unwrap_err());
    assert!(malformed.starts_with("decoding response: "), "{malformed}");
}

/// Mining journals do real file locking, so tests need an isolated root
/// instead of sharing one.
fn runtime_with_instance_dir(instance: &InstanceName) -> (tempfile::TempDir, Runtime) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    fs::create_dir_all(runtime.instance_dir(instance)).unwrap();
    (dir, runtime)
}

#[test]
fn mine_polls_until_completion_and_reports_the_confirmed_count() {
    let (url, requests) = serve(vec![
        (
            "202 Accepted",
            r#"{"id":"job-1","requested_blocks":3,"completed_blocks":0,"state":"mining","error":null,"progress_uncertain":false}"#,
        ),
        (
            "200 OK",
            r#"{"id":"job-1","requested_blocks":3,"completed_blocks":1,"state":"mining","error":null,"progress_uncertain":false}"#,
        ),
        (
            "200 OK",
            r#"{"id":"job-1","requested_blocks":3,"completed_blocks":3,"state":"completed","error":null,"progress_uncertain":false,"hashes":["a","b","c"]}"#,
        ),
    ]);
    let client = reqwest::blocking::Client::new();
    let alpha = name("alpha");
    let (_dir, runtime) = runtime_with_instance_dir(&alpha);

    let mut out = Vec::new();
    runtime
        .mine_with(
            &alpha,
            &url,
            &client,
            &mut out,
            Instant::now() + Duration::from_secs(5),
            3,
            false,
        )
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("New tip: c"), "{text}");

    // The POST carries a JSON body; the two GET polls that follow carry none.
    let mut requests = requests.try_iter();
    let post: serde_json::Value = serde_json::from_str(&requests.next().unwrap().1).unwrap();
    assert_eq!(post["blocks"], 3);
    assert!(post["idempotency_key"].is_string());
    assert_eq!(requests.count(), 2, "expected exactly two GET polls");

    // A completed job's key is forgotten, so mining the same count again
    // starts a fresh job rather than replaying this frozen one.
    assert!(
        IntentJournal::open(&runtime.instance_dir(&alpha), "mining")
            .unwrap()
            .read()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn mine_reports_partial_progress_alongside_the_servers_error() {
    let (url, _requests) = serve(vec![
        (
            "202 Accepted",
            r#"{"id":"job-1","requested_blocks":5,"completed_blocks":0,"state":"mining","error":null,"progress_uncertain":false}"#,
        ),
        (
            "200 OK",
            r#"{"id":"job-1","requested_blocks":5,"completed_blocks":2,"state":"failed","error":"generate failed","progress_uncertain":true}"#,
        ),
    ]);
    let client = reqwest::blocking::Client::new();
    let alpha = name("alpha");
    let (_dir, runtime) = runtime_with_instance_dir(&alpha);

    let mut out = Vec::new();
    let error = runtime
        .mine_with(
            &alpha,
            &url,
            &client,
            &mut out,
            Instant::now() + Duration::from_secs(5),
            5,
            true,
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "generate failed");

    let output = json_stdout(&out);
    assert_eq!(output["id"], "job-1");
    assert_eq!(output["blocks"], 2);
    assert_eq!(output["requested_blocks"], 5);
    assert_eq!(output["state"], "failed");
    assert_eq!(output["progress_uncertain"], true);
    assert_eq!(output["error"], "generate failed");

    // A terminal job, failed or not, is forgotten the same way.
    assert!(
        IntentJournal::open(&runtime.instance_dir(&alpha), "mining")
            .unwrap()
            .read()
            .unwrap()
            .is_empty()
    );
}

/// Always fails to write, as a broken pipe or a full disk would.
struct FailingWriter;

impl std::io::Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("the pipe is closed"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn mine_keeps_the_journal_key_when_writing_the_json_result_fails() {
    let (url, _requests) = serve(vec![(
        "202 Accepted",
        r#"{"id":"job-1","requested_blocks":1,"completed_blocks":1,"state":"completed","error":null,"progress_uncertain":false,"hashes":["tip"]}"#,
    )]);
    let client = reqwest::blocking::Client::new();
    let alpha = name("alpha");
    let (_dir, runtime) = runtime_with_instance_dir(&alpha);

    runtime
        .mine_with(
            &alpha,
            &url,
            &client,
            FailingWriter,
            Instant::now() + Duration::from_secs(5),
            1,
            true,
        )
        .unwrap_err();

    // The job completed, but its result was never actually delivered, so the
    // key must survive: clearing it here would let a retry start a second
    // job for output the caller never received.
    let saved = IntentJournal::open(&runtime.instance_dir(&alpha), "mining")
        .unwrap()
        .read()
        .unwrap();
    assert_eq!(saved.len(), 1);
    assert!(saved.contains_key("mine:1"));
}

#[test]
fn submission_retries_pause_between_attempts_instead_of_busy_looping() {
    // Each attempt fails instantly (a dropped connection, not a stall), so
    // without a pause between attempts this would retry as fast as the
    // network allows instead of at a reasonable rate.
    let (url, requests) = serve(vec![("drop", ""); 20]);
    let client = reqwest::blocking::Client::new();
    let alpha = name("alpha");
    let (_dir, runtime) = runtime_with_instance_dir(&alpha);

    let mut out = Vec::new();
    let error = runtime
        .mine_with(
            &alpha,
            &url,
            &client,
            &mut out,
            Instant::now() + Duration::from_millis(900),
            1,
            false,
        )
        .unwrap_err();
    let full_error = format!("{error:#}");
    assert!(
        full_error.contains("may have been accepted"),
        "{full_error}"
    );

    let attempts = requests.try_iter().count();
    assert!(
        attempts <= 6,
        "expected a handful of paused attempts within ~900ms, got {attempts}"
    );
}

#[test]
fn mine_status_line_reports_progress_for_every_state() {
    let mining = MiningJob {
        id: "job-1".into(),
        requested_blocks: 5,
        completed_blocks: 2,
        state: MiningJobState::Mining,
        error: None,
        progress_uncertain: false,
        hashes: None,
    };
    assert_eq!(
        mine_status_line(&mining, &name("alpha")),
        "2 of 5 blocks confirmed on alpha. Mining blocks…"
    );

    let syncing = MiningJob {
        state: MiningJobState::Syncing,
        completed_blocks: 5,
        ..mining.clone()
    };
    assert_eq!(
        mine_status_line(&syncing, &name("alpha")),
        "5 of 5 blocks confirmed on alpha. Synchronizing wallet…"
    );

    let completed_without_hashes = MiningJob {
        state: MiningJobState::Completed,
        ..syncing.clone()
    };
    assert_eq!(
        mine_status_line(&completed_without_hashes, &name("alpha")),
        "5 of 5 blocks confirmed on alpha. Mining completed. Wallet synchronized."
    );

    let completed = MiningJob {
        hashes: Some(vec!["aaa".into(), "the-tip".into()]),
        ..completed_without_hashes
    };
    assert_eq!(
        mine_status_line(&completed, &name("alpha")),
        "5 of 5 blocks confirmed on alpha. Mining completed. Wallet synchronized. New tip: the-tip"
    );

    let uncertain_failure = MiningJob {
        state: MiningJobState::Failed,
        completed_blocks: 1,
        progress_uncertain: true,
        ..mining
    };
    assert_eq!(
        mine_status_line(&uncertain_failure, &name("alpha")),
        "At least 1 of 5 blocks confirmed on alpha; the last mining result is uncertain. Mining job job-1 failed."
    );
}

#[test]
fn a_dropped_poll_connection_is_retried_within_the_deadline() {
    // Distinct from a stalled poll: the connection is reset immediately, not
    // held open past the attempt timeout. The job may still be running on
    // the server, so a disconnected poll must not be treated as the job
    // itself having failed.
    let (url, requests) = serve(vec![
        (
            "202 Accepted",
            r#"{"id":"job-1","requested_blocks":1,"completed_blocks":0,"state":"mining","error":null,"progress_uncertain":false}"#.to_owned(),
        ),
        ("drop", String::new()),
        (
            "200 OK",
            r#"{"id":"job-1","requested_blocks":1,"completed_blocks":1,"state":"completed","error":null,"progress_uncertain":false,"hashes":["tip"]}"#.to_owned(),
        ),
    ]);
    let client = reqwest::blocking::Client::new();
    let alpha = name("alpha");
    let (_dir, runtime) = runtime_with_instance_dir(&alpha);

    let started = Instant::now();
    let mut out = Vec::new();
    runtime
        .mine_with(
            &alpha,
            &url,
            &client,
            &mut out,
            Instant::now() + Duration::from_secs(10),
            1,
            false,
        )
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a dropped poll connection should be retried almost immediately: {:?}",
        started.elapsed()
    );
    assert_eq!(requests.try_iter().count(), 3);
}

#[test]
fn a_stalled_poll_times_out_within_the_deadline_instead_of_hanging() {
    // The server holds the connection open without answering. A single
    // stalled request must not itself consume the whole deadline: the
    // per-attempt timeout bounds it well below that.
    let (url, _requests) = serve(vec![
        (
            "202 Accepted",
            r#"{"id":"job-1","requested_blocks":1,"completed_blocks":0,"state":"mining","error":null,"progress_uncertain":false}"#.to_owned(),
        ),
        ("hang", String::new()),
    ]);
    let client = reqwest::blocking::Client::new();
    let alpha = name("alpha");
    let (_dir, runtime) = runtime_with_instance_dir(&alpha);

    let started = Instant::now();
    let mut out = Vec::new();
    let error = runtime
        .mine_with(
            &alpha,
            &url,
            &client,
            &mut out,
            Instant::now() + Duration::from_millis(500),
            1,
            false,
        )
        .unwrap_err();
    assert!(error.to_string().contains("job-1"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a stalled poll must not block anywhere close to the overall deadline: {:?}",
        started.elapsed()
    );

    // Still running, not a terminal state: the key survives so a retry picks
    // the same job back up instead of starting a second one.
    let saved = IntentJournal::open(&runtime.instance_dir(&alpha), "mining")
        .unwrap()
        .read()
        .unwrap();
    assert_eq!(saved.len(), 1);
    assert!(saved.contains_key("mine:1"));
}

#[test]
fn an_unresolved_submission_key_survives_for_the_next_invocation() {
    // The initial POST fails outright (no response at all), the failure mode
    // a same-process retry cannot paper over once its own deadline is spent.
    // The next `ths mine` with the same block count must still find and
    // reuse the saved key rather than minting a new one.
    let unreachable = "http://127.0.0.1:1";
    let client = reqwest::blocking::Client::new();
    let alpha = name("alpha");
    let (_dir, runtime) = runtime_with_instance_dir(&alpha);

    let mut out = Vec::new();
    runtime
        .mine_with(
            &alpha,
            unreachable,
            &client,
            &mut out,
            Instant::now() + Duration::from_millis(300),
            5,
            false,
        )
        .unwrap_err();

    let journal = IntentJournal::open(&runtime.instance_dir(&alpha), "mining").unwrap();
    let first_attempt_key = journal.read().unwrap().get("mine:5").unwrap().clone();
    drop(journal);

    let (url, requests) = serve(vec![(
        "202 Accepted",
        r#"{"id":"job-1","requested_blocks":5,"completed_blocks":5,"state":"completed","error":null,"progress_uncertain":false,"hashes":["a"]}"#,
    )]);
    let mut out = Vec::new();
    runtime
        .mine_with(
            &alpha,
            &url,
            &client,
            &mut out,
            Instant::now() + Duration::from_secs(5),
            5,
            false,
        )
        .unwrap();

    let post: serde_json::Value =
        serde_json::from_str(&requests.try_iter().next().unwrap().1).unwrap();
    assert_eq!(post["idempotency_key"], first_attempt_key);
}

#[test]
fn activity_output_distinguishes_routes_and_hides_the_treasury() {
    let activity: Vec<serde_json::Value> = serde_json::from_str(
        r#"[
            {"id":"f","kind":"faucet","from_account":6,"to_account":1,"to_address":null,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":125000000,"txid":"faucet-tx","block_hash":"faucet-block","status":"confirmed","created_at":"2026-10-12 10:30:00","last_error":null},
            {"id":"s","kind":"send","from_account":1,"to_account":2,"to_address":null,"source_pool":"transparent","destination_pool":"ironwood","amount_zatoshi":50000000,"txid":"shield-tx","block_hash":null,"status":"broadcast","created_at":"2026-10-12 10:31:00","last_error":null},
            {"id":"u","kind":"send","from_account":2,"to_account":3,"to_address":null,"source_pool":"ironwood","destination_pool":"transparent","amount_zatoshi":25000000,"txid":"unshield-tx","block_hash":null,"status":"preparing","created_at":"2026-10-12 10:32:00","last_error":"wallet is still syncing"},
            {"id":"e","kind":"send","from_account":4,"to_account":null,"to_address":"uregtest1external","source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":10000000,"txid":"","block_hash":null,"status":"preparing","created_at":"2026-10-12 10:33:00","last_error":null}
        ]"#,
    )
    .unwrap();

    let output = activity_lines(&activity).unwrap();
    for expected in [
        "faucet  confirmed  1.25 ZEC",
        "faucet (ironwood) -> account 1 (ironwood)",
        "shield  broadcast  0.5 ZEC",
        "unshield  preparing  0.25 ZEC",
        "account 4 (ironwood) -> uregtest1external (ironwood)",
        "Transaction: faucet-tx",
        "Confirmed in: faucet-block",
        "Error: wallet is still syncing",
        "Created: 2026-10-12 10:30:00",
    ] {
        assert!(
            output.contains(expected),
            "missing `{expected}` from {output}"
        );
    }
    assert!(!output.contains("account 6"));
    assert!(!output.contains("Transaction: \n"));
}

#[test]
fn activity_output_handles_empty_history_and_preserves_json_fields() {
    assert_eq!(activity_lines(&[]).unwrap(), "No wallet activity yet.");

    let activity = vec![serde_json::json!({
        "id": "a",
        "kind": "send",
        "from_account": 1,
        "to_account": null,
        "to_address": "uregtest1external",
        "source_pool": "ironwood",
        "destination_pool": "ironwood",
        "amount_zatoshi": 10,
        "txid": "",
        "block_hash": null,
        "status": "preparing",
        "created_at": "2026-10-12 10:30:00",
        "last_error": "wallet is still syncing",
        "future_server_field": {"kept": true},
    })];

    let output = render_activity_history(&activity, true).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&output).unwrap(),
        serde_json::Value::Array(activity)
    );
}

#[test]
fn activity_history_forwards_only_explicit_limits() {
    for (limit, expected_request) in [
        (None, "GET /api/v1/activity HTTP/1.1"),
        (Some(20), "GET /api/v1/activity?limit=20 HTTP/1.1"),
    ] {
        let (url, requests) = serve(vec![("200 OK", "[]")]);
        let activity = activity_history(
            &url,
            &reqwest::blocking::Client::new(),
            &name("alpha"),
            limit,
        )
        .unwrap();

        assert!(activity.is_empty());
        assert_eq!(requests.recv().unwrap().0, expected_request);
    }
}

#[test]
fn wallet_faucet_keeps_funding_after_a_failed_account_and_keeps_its_keys() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let alpha = name("alpha");
    fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
    let pending = r#"{"id":"a","kind":"faucet","from_account":null,"to_account":2,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":100,"txid":"t","block_hash":null,"status":"broadcast"}"#;
    let funded = r#"{"id":"b","kind":"faucet","from_account":null,"to_account":3,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":100,"txid":"u","block_hash":"b","status":"confirmed"}"#;
    let (url, requests) = serve(vec![
        ("422 Unprocessable Entity", "no funds"),
        ("200 OK", pending),
        ("200 OK", funded),
    ]);

    let mut out = Vec::new();
    let error = runtime
        .wallet_faucet_with(
            &alpha,
            &url,
            &reqwest::blocking::Client::new(),
            &mut out,
            Instant::now() + Duration::from_millis(500),
            &[1, 2, 3],
            100,
            "ironwood",
            true,
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "2 of 3 faucet requests are pending or failed"
    );
    let output = json_stdout(&out);
    assert_eq!(output["funded"][0]["to_account"], 3);
    assert_eq!(output["pending"][0]["to_account"], 2);
    assert!(
        output["pending"][0]["idempotency_key"].is_string(),
        "{output}"
    );
    assert_eq!(output["failed"].as_array().unwrap().len(), 1);

    let requests: Vec<serde_json::Value> = requests
        .try_iter()
        .map(|(_, body)| serde_json::from_str(&body).unwrap())
        .collect();
    assert_eq!(requests.len(), 3);
    let journal = IntentJournal::open(&runtime.instance_dir(&alpha), "faucet").unwrap();
    for (account, request) in (1..=3).zip(&requests) {
        assert_eq!(request["account_id"], account);
        // the unfinished batch keeps every key, so a retry submits the same ones.
        assert_eq!(
            request["idempotency_key"],
            journal
                .key_for(&format!("wallet:ironwood:100:1,2,3:{account}"))
                .unwrap()
        );
    }
}

#[test]
fn balances_print_each_account_without_viewing_keys() {
    let accounts: Vec<Account> = serde_json::from_str(
        r#"[{"id":1,"name":"Account 1","unified_address":"uregtest1a","transparent_address":"tm1a","unified_full_viewing_key":"uviewregtest1a","transparent_zatoshi":50000000,"ironwood_zatoshi":500000000},
            {"id":2,"name":"Account 2","unified_address":"uregtest1b","transparent_address":"tm1b","unified_full_viewing_key":"uviewregtest1b","transparent_zatoshi":0,"ironwood_zatoshi":1}]"#,
    )
    .unwrap();
    assert_eq!(
        balance_lines(&accounts),
        "Account 1\n  Transparent  0.5 ZEC  tm1a\n  Ironwood     5 ZEC  uregtest1a\n\
         Account 2\n  Transparent  0 ZEC  tm1b\n  Ironwood     0.00000001 ZEC  uregtest1b"
    );
    assert_eq!(
        serde_json::to_value(&accounts).unwrap()[0],
        serde_json::json!({
            "id": 1,
            "transparent_address": "tm1a",
            "unified_address": "uregtest1a",
            "transparent_zatoshi": 50_000_000,
            "ironwood_zatoshi": 500_000_000,
        })
    );
}

#[test]
fn balances_require_a_ready_wallet_before_reading_accounts() {
    let (url, requests) = serve(vec![
        (
            "200 OK",
            r#"{"wallet_sync":{"state":"ready","error":null}}"#,
        ),
        (
            "200 OK",
            r#"[{"id":1,"unified_address":"uregtest1a","transparent_address":"tm1a","transparent_zatoshi":50000000,"ironwood_zatoshi":500000000}]"#,
        ),
    ]);

    let accounts =
        account_balances(&url, &reqwest::blocking::Client::new(), &name("alpha")).unwrap();

    assert_eq!(accounts.len(), 1);
    assert_eq!(requests.try_iter().count(), 2);
}

#[test]
fn balances_reject_syncing_and_failed_wallets_before_reading_accounts() {
    for (status, expected) in [
        (
            r#"{"wallet_sync":{"state":"syncing","error":null}}"#,
            "wallet for environment alpha is still syncing; try again later",
        ),
        (
            r#"{"wallet_sync":{"state":"error","error":"lightwalletd is unavailable"}}"#,
            "wallet synchronization failed for environment alpha: lightwalletd is unavailable",
        ),
    ] {
        let (url, requests) = serve(vec![("200 OK", status)]);

        let error =
            account_balances(&url, &reqwest::blocking::Client::new(), &name("alpha")).unwrap_err();

        assert_eq!(error.to_string(), expected);
        assert_eq!(requests.try_iter().count(), 1);
    }
}

#[test]
fn uri_sends_resolve_the_amount_before_submission() {
    const WITH_AMOUNT: &str = r#"{"address":"uregtest1external","destination_pool":"ironwood","amount_zatoshi":123,"memo":"hi"}"#;
    const NO_AMOUNT: &str = r#"{"address":"uregtest1external","destination_pool":"ironwood","amount_zatoshi":null,"memo":"hi"}"#;
    const CONFIRMED: &str = r#"{"id":"uri-send","kind":"send","from_account":1,"to_account":null,"to_address":"uregtest1external","source_pool":"transparent","destination_pool":"ironwood","amount_zatoshi":123,"txid":"uri-txid","block_hash":"block","status":"confirmed","last_error":null}"#;
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let alpha = name("alpha");
    fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
    for (parsed, amount, valid) in [
        (WITH_AMOUNT, Some(TransferAmount::Exact(123)), false),
        (NO_AMOUNT, None, false),
        (WITH_AMOUNT, None, true),
        (NO_AMOUNT, Some(TransferAmount::Exact(123)), true),
    ] {
        let mut responses = vec![("200 OK", parsed)];
        if valid {
            responses.push(("200 OK", CONFIRMED));
        }
        let (url, requests) = serve(responses);
        let result = runtime.wallet_send_uri_with(
            &alpha,
            &url,
            &reqwest::blocking::Client::new(),
            1,
            "transparent",
            "zcash:uregtest1external",
            amount,
            true,
        );
        assert_eq!(result.is_ok(), valid, "{result:?}");
        let bodies: Vec<serde_json::Value> = requests
            .try_iter()
            .map(|(_, body)| serde_json::from_str(&body).unwrap())
            .collect();
        assert_eq!(bodies.len(), if valid { 2 } else { 1 });
        assert_eq!(
            bodies[0],
            serde_json::json!({"uri":"zcash:uregtest1external"})
        );
        if valid {
            assert_eq!(bodies[1]["to_address"], "uregtest1external");
            assert_eq!(bodies[1]["source_pool"], "transparent");
            assert_eq!(bodies[1]["destination_pool"], "ironwood");
            assert_eq!(bodies[1]["amount_zatoshi"], 123);
            assert_eq!(bodies[1]["memo"], "hi");
            assert!(bodies[1].get("to_account").is_none());
        }
    }
    for (status, response) in [
        ("400 Bad Request", "invalid URI"),
        ("200 OK", "not json"),
        ("200 OK", r#"{"address":"missing other fields"}"#),
    ] {
        let (url, requests) = serve(vec![(status, response)]);
        assert!(
            runtime
                .wallet_send_uri_with(
                    &alpha,
                    &url,
                    &reqwest::blocking::Client::new(),
                    1,
                    "transparent",
                    "invalid",
                    None,
                    true
                )
                .is_err()
        );
        assert_eq!(requests.try_iter().count(), 1);
    }
}

#[test]
fn max_send_quotes_then_submits_the_quoted_amount() {
    let (_dir, runtime, alpha) = payment_runtime();
    let confirmed = r#"{"id":"max-send","kind":"send","from_account":1,"to_account":2,"to_address":null,"source_pool":"ironwood","destination_pool":"transparent","amount_zatoshi":490,"txid":"max-txid","block_hash":"block","status":"confirmed","last_error":null}"#;
    let (url, requests) = serve(vec![
        (
            "200 OK",
            r#"{"available_zatoshi":500,"fee_zatoshi":10,"max_zatoshi":490}"#,
        ),
        ("200 OK", confirmed),
    ]);
    let mut out = Vec::new();

    runtime
        .wallet_send_with(
            &alpha,
            &url,
            &reqwest::blocking::Client::new(),
            &mut out,
            Instant::now() + Duration::from_secs(5),
            1,
            2,
            "ironwood",
            "transparent",
            TransferAmount::Max,
            None,
            true,
        )
        .unwrap();

    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].0.starts_with("POST /api/v1/send/quote "));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&requests[0].1).unwrap(),
        serde_json::json!({
            "from_account": 1,
            "source_pool": "ironwood",
            "destination_pool": "transparent",
        })
    );
    assert!(requests[1].0.starts_with("POST /api/v1/send "));
    let submitted: serde_json::Value = serde_json::from_str(&requests[1].1).unwrap();
    assert_eq!(submitted["amount_zatoshi"], 490);
    assert_eq!(json_stdout(&out)["amount_zatoshi"], 490);
}

#[test]
fn pending_max_send_reuses_its_quote_and_idempotency_key() {
    let (_dir, runtime, alpha) = payment_runtime();
    let pending = r#"{"id":"max-send","kind":"send","from_account":1,"to_account":2,"to_address":null,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":490,"txid":"max-txid","block_hash":null,"status":"broadcast","last_error":null}"#;
    let (url, requests) = serve(vec![
        (
            "200 OK",
            r#"{"available_zatoshi":500,"fee_zatoshi":10,"max_zatoshi":490}"#,
        ),
        ("200 OK", pending),
    ]);
    let mut out = Vec::new();

    runtime
        .wallet_send_with(
            &alpha,
            &url,
            &reqwest::blocking::Client::new(),
            &mut out,
            Instant::now() + Duration::from_millis(500),
            1,
            2,
            "ironwood",
            "ironwood",
            TransferAmount::Max,
            None,
            true,
        )
        .unwrap_err();
    let first: Vec<_> = requests.try_iter().collect();
    assert_eq!(first.len(), 2);
    let original_key = sent_key(&first[1]);

    let confirmed = r#"{"id":"max-send","kind":"send","from_account":1,"to_account":2,"to_address":null,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":490,"txid":"max-txid","block_hash":"block","status":"confirmed","last_error":null}"#;
    let (url, requests) = serve(vec![("200 OK", confirmed)]);
    out.clear();
    runtime
        .wallet_send_with(
            &alpha,
            &url,
            &reqwest::blocking::Client::new(),
            &mut out,
            Instant::now() + Duration::from_secs(5),
            1,
            2,
            "ironwood",
            "ironwood",
            TransferAmount::Max,
            None,
            true,
        )
        .unwrap();

    let retried: Vec<_> = requests.try_iter().collect();
    assert_eq!(retried.len(), 1, "a retry must not request a new quote");
    assert!(retried[0].0.starts_with("POST /api/v1/send "));
    assert_eq!(sent_key(&retried[0]), original_key);
    let body: serde_json::Value = serde_json::from_str(&retried[0].1).unwrap();
    assert_eq!(body["amount_zatoshi"], 490);
}

#[test]
fn zero_max_does_not_submit_a_payment() {
    let (_dir, runtime, alpha) = payment_runtime();
    let (url, requests) = serve(vec![(
        "200 OK",
        r#"{"available_zatoshi":10,"fee_zatoshi":10,"max_zatoshi":0}"#,
    )]);
    let mut out = Vec::new();

    let error = runtime
        .wallet_send_with(
            &alpha,
            &url,
            &reqwest::blocking::Client::new(),
            &mut out,
            Instant::now() + Duration::from_secs(5),
            1,
            2,
            "ironwood",
            "ironwood",
            TransferAmount::Max,
            None,
            true,
        )
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "account 1 has no spendable ironwood funds remaining after the network fee"
    );
    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].0.starts_with("POST /api/v1/send/quote "));
    assert!(out.is_empty());
}

#[test]
fn max_quote_errors_are_reported_without_submitting() {
    let (_dir, runtime, alpha) = payment_runtime();
    let (url, requests) = serve(vec![("503 Service Unavailable", "wallet is syncing")]);
    let mut out = Vec::new();

    let error = runtime
        .wallet_send_with(
            &alpha,
            &url,
            &reqwest::blocking::Client::new(),
            &mut out,
            Instant::now() + Duration::from_secs(5),
            1,
            2,
            "ironwood",
            "ironwood",
            TransferAmount::Max,
            None,
            true,
        )
        .unwrap_err();

    assert!(
        format!("{error:#}").contains(
            "asking environment alpha for the maximum transfer amount: rejected (503 Service Unavailable): wallet is syncing"
        ),
        "{error:#}"
    );
    assert_eq!(requests.try_iter().count(), 1);
    assert!(out.is_empty());
}

#[test]
fn delayed_max_quote_respects_the_payment_deadline_without_saving_an_intent() {
    let (_dir, runtime, alpha) = payment_runtime();
    let (url, requests) = serve(vec![
        (
            "delay",
            r#"{"available_zatoshi":500,"fee_zatoshi":10,"max_zatoshi":490}"#.to_owned(),
        ),
        ("200 OK", activity_json("confirmed", "txid", None)),
    ]);
    let mut out = Vec::new();

    let error = runtime
        .wallet_send_with(
            &alpha,
            &url,
            &reqwest::blocking::Client::new(),
            &mut out,
            Instant::now() + Duration::from_millis(50),
            1,
            2,
            "ironwood",
            "ironwood",
            TransferAmount::Max,
            None,
            true,
        )
        .unwrap_err();

    let error = format!("{error:#}");
    assert!(error.contains("maximum transfer amount"), "{error}");
    assert!(error.contains("timed out"), "{error}");
    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].0.starts_with("POST /api/v1/send/quote "));
    let journal = IntentJournal::open(&runtime.instance_dir(&alpha), "send").unwrap();
    assert!(journal.read().unwrap().is_empty());
    assert!(out.is_empty());
}

#[test]
fn expired_max_deadline_does_not_quote_submit_or_save_an_intent() {
    let (_dir, runtime, alpha) = payment_runtime();
    let (url, requests) = serve(vec![(
        "200 OK",
        r#"{"available_zatoshi":500,"fee_zatoshi":10,"max_zatoshi":490}"#,
    )]);
    let mut out = Vec::new();

    let error = runtime
        .wallet_send_with(
            &alpha,
            &url,
            &reqwest::blocking::Client::new(),
            &mut out,
            Instant::now() - Duration::from_millis(1),
            1,
            2,
            "ironwood",
            "ironwood",
            TransferAmount::Max,
            None,
            true,
        )
        .unwrap_err();

    let error = format!("{error:#}");
    assert!(error.contains("maximum transfer amount"), "{error}");
    assert!(error.contains("quote timed out"), "{error}");
    assert!(requests.try_recv().is_err());
    let journal = IntentJournal::open(&runtime.instance_dir(&alpha), "send").unwrap();
    assert!(journal.read().unwrap().is_empty());
    assert!(out.is_empty());
}

#[test]
fn uri_max_uses_the_parsed_destination_pool_for_the_quote() {
    const PARSED: &str = r#"{"address":"uregtest1external","destination_pool":"ironwood","amount_zatoshi":null,"memo":"hi"}"#;
    const CONFIRMED: &str = r#"{"id":"uri-max","kind":"send","from_account":1,"to_account":null,"to_address":"uregtest1external","source_pool":"transparent","destination_pool":"ironwood","amount_zatoshi":123,"txid":"uri-txid","block_hash":"block","status":"confirmed","last_error":null}"#;
    let (_dir, runtime, alpha) = payment_runtime();
    let (url, requests) = serve(vec![
        ("200 OK", PARSED),
        (
            "200 OK",
            r#"{"available_zatoshi":133,"fee_zatoshi":10,"max_zatoshi":123}"#,
        ),
        ("200 OK", CONFIRMED),
    ]);

    runtime
        .wallet_send_uri_with(
            &alpha,
            &url,
            &reqwest::blocking::Client::new(),
            1,
            "transparent",
            "zcash:uregtest1external",
            Some(TransferAmount::Max),
            true,
        )
        .unwrap();

    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].0.starts_with("POST /api/v1/send/quote "));
    let quote: serde_json::Value = serde_json::from_str(&requests[1].1).unwrap();
    assert_eq!(quote["source_pool"], "transparent");
    assert_eq!(quote["destination_pool"], "ironwood");
    let submitted: serde_json::Value = serde_json::from_str(&requests[2].1).unwrap();
    assert_eq!(submitted["amount_zatoshi"], 123);
}

#[test]
fn uri_amount_rejects_max_before_quoting_or_submitting() {
    const PARSED: &str = r#"{"address":"uregtest1external","destination_pool":"ironwood","amount_zatoshi":123,"memo":null}"#;
    let (_dir, runtime, alpha) = payment_runtime();
    let (url, requests) = serve(vec![("200 OK", PARSED)]);

    let error = runtime
        .wallet_send_uri_with(
            &alpha,
            &url,
            &reqwest::blocking::Client::new(),
            1,
            "ironwood",
            "zcash:uregtest1external?amount=0.00000123",
            Some(TransferAmount::Max),
            true,
        )
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "the payment URI already sets an amount; omit --max"
    );
    assert_eq!(requests.try_iter().count(), 1);
}

fn json_stdout(out: &[u8]) -> serde_json::Value {
    if out.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_slice(out).unwrap_or_else(|error| {
        panic!(
            "JSON stdout is not one JSON value ({error}): {}",
            String::from_utf8_lossy(out)
        )
    })
}

fn activity_json(status: &str, txid: &str, last_error: Option<&str>) -> String {
    let block_hash = (status == "confirmed").then_some("block");
    serde_json::json!({
        "id": "op", "kind": "send", "from_account": 1, "to_account": 2,
        "source_pool": "ironwood", "destination_pool": "ironwood", "amount_zatoshi": 100,
        "txid": txid, "block_hash": block_hash, "status": status, "last_error": last_error,
    })
    .to_string()
}

fn send_intent(memo: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "from_account": 1, "to_account": 2, "source_pool": "ironwood",
        "destination_pool": "ironwood", "amount_zatoshi": 100, "memo": memo,
    })
}

fn payment_runtime() -> (tempfile::TempDir, Runtime, InstanceName) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let alpha = name("alpha");
    fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
    (dir, runtime, alpha)
}

fn send_with(
    runtime: &Runtime,
    alpha: &InstanceName,
    url: &str,
    wait: Duration,
    intent: &serde_json::Value,
) -> (Result<()>, serde_json::Value) {
    let mut out = Vec::new();
    let result = runtime.submit_send(
        alpha,
        url,
        &reqwest::blocking::Client::new(),
        &mut out,
        Instant::now() + wait,
        1,
        intent.clone(),
        intent["memo"].as_str(),
        true,
    );
    (result, json_stdout(&out))
}

fn saved_send_key(
    runtime: &Runtime,
    alpha: &InstanceName,
    intent: &serde_json::Value,
) -> Option<String> {
    let journal = IntentJournal::open(&runtime.instance_dir(alpha), "send").unwrap();
    journal
        .read()
        .unwrap()
        .get(&serde_json::to_string(intent).unwrap())
        .cloned()
}

fn sent_key(request: &(String, String)) -> String {
    let body: serde_json::Value = serde_json::from_str(&request.1).unwrap();
    body["idempotency_key"].as_str().unwrap().to_owned()
}

#[test]
fn send_fingerprints_distinguish_every_field_including_an_empty_memo() {
    let base = serde_json::to_string(&send_intent(None)).unwrap();
    let with = |field: &str, value: serde_json::Value| {
        let mut body = send_intent(None);
        body[field] = value;
        body
    };
    let mut to_address = send_intent(None);
    to_address.as_object_mut().unwrap().remove("to_account");
    to_address["to_address"] = "tmRegtestRecipient".into();
    let variants = [
        with("from_account", 3.into()),
        with("to_account", 3.into()),
        with("source_pool", "transparent".into()),
        with("destination_pool", "transparent".into()),
        with("amount_zatoshi", 101.into()),
        send_intent(Some("")),
        send_intent(Some("hi")),
        to_address,
    ];
    let mut fingerprints = std::collections::BTreeSet::from([base.clone()]);
    for variant in &variants {
        assert!(fingerprints.insert(serde_json::to_string(variant).unwrap()));
    }
    // serde_json sorts object keys, so building the same request in another field order keeps
    // its identity.
    let reordered = serde_json::json!({
        "memo": null, "amount_zatoshi": 100, "destination_pool": "ironwood",
        "source_pool": "ironwood", "to_account": 2, "from_account": 1,
    });
    assert_eq!(serde_json::to_string(&reordered).unwrap(), base);
}

#[test]
fn send_journal_failure_prevents_submission() {
    let (_dir, runtime, alpha) = payment_runtime();
    fs::create_dir(runtime.instance_dir(&alpha).join("send-intents.json")).unwrap();
    let (url, requests) = serve(vec![("200 OK", activity_json("confirmed", "t", None))]);

    let (result, output) = send_with(
        &runtime,
        &alpha,
        &url,
        Duration::from_secs(1),
        &send_intent(None),
    );
    let error = result.unwrap_err();

    assert!(
        format!("{error:#}").contains("reading saved send intents"),
        "{error:#}"
    );
    assert!(
        requests.try_recv().is_err(),
        "submitted without a retained key"
    );
    assert_eq!(output, serde_json::Value::Null);
}

#[test]
fn send_key_survives_an_interrupted_process_and_resumes_the_same_payment() {
    let (_dir, runtime, alpha) = payment_runtime();
    let intent = send_intent(Some(""));
    let (url, requests) = serve(vec![("hang", String::new())]);
    let mut interrupted = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runtime::tests::send_interruption_probe"])
        .env("THS_SEND_PROBE_ROOT", &runtime.root)
        .env("THS_SEND_PROBE_URL", &url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let submitted = requests.recv_timeout(Duration::from_secs(10)).unwrap();
    interrupted.kill().unwrap();
    let interrupted = interrupted.wait_with_output().unwrap();
    assert!(!interrupted.status.success(), "probe was not interrupted");
    let probe_output = String::from_utf8(interrupted.stdout).unwrap();
    assert!(
        probe_output.contains("running 1 test"),
        "expected one selected interruption probe: {probe_output}"
    );
    let key = sent_key(&submitted);
    assert_eq!(saved_send_key(&runtime, &alpha, &intent), Some(key.clone()));

    let (url, requests) = serve(vec![("200 OK", activity_json("confirmed", "t", None))]);
    let (result, output) = send_with(&runtime, &alpha, &url, Duration::from_secs(5), &intent);
    result.unwrap();
    assert_eq!(output["status"], "confirmed");
    assert!(output.get("idempotency_key").is_none(), "{output}");
    assert_eq!(sent_key(&requests.recv().unwrap()), key);
    assert_eq!(saved_send_key(&runtime, &alpha, &intent), None);

    // A deliberate identical payment after observed confirmation gets a new identity.
    let (url, requests) = serve(vec![("200 OK", activity_json("confirmed", "u", None))]);
    send_with(&runtime, &alpha, &url, Duration::from_secs(5), &intent)
        .0
        .unwrap();
    assert_ne!(sent_key(&requests.recv().unwrap()), key);
}

#[test]
fn lost_send_response_follows_the_key_through_a_replaced_transaction() {
    let (_dir, runtime, alpha) = payment_runtime();
    let intent = send_intent(None);
    let (url, requests) = serve(vec![
        ("drop", String::new()),
        (
            "200 OK",
            format!("[{}]", activity_json("broadcast", "first", None)),
        ),
        (
            "200 OK",
            format!("[{}]", activity_json("confirmed", "replacement", None)),
        ),
    ]);

    let (result, output) = send_with(&runtime, &alpha, &url, Duration::from_secs(10), &intent);
    result.unwrap();
    assert_eq!(output["txid"], "replacement");

    let requests: Vec<_> = requests.try_iter().collect();
    let key = sent_key(&requests[0]);
    assert!(requests[0].0.starts_with("POST /api/v1/send "));
    for (line, _) in &requests[1..] {
        assert!(
            line.starts_with(&format!("GET /api/v1/activity?idempotency_key={key} ")),
            "{line}"
        );
    }
    assert_eq!(requests.len(), 3, "status reads must not resubmit");
    assert_eq!(saved_send_key(&runtime, &alpha, &intent), None);
}

#[test]
fn unavailable_status_keeps_the_key_and_is_not_a_failure() {
    let (_dir, runtime, alpha) = payment_runtime();
    let intent = send_intent(None);
    let (url, requests) = serve(vec![
        ("drop", String::new()),
        ("503 Service Unavailable", "node down".to_owned()),
    ]);

    let (result, output) = send_with(&runtime, &alpha, &url, Duration::from_millis(1500), &intent);

    let error = format!("{:#}", result.unwrap_err());
    assert!(error.contains("run the same command"), "{error}");
    assert!(!error.contains("failed:"), "{error}");
    let key = sent_key(&requests.recv().unwrap());
    assert_eq!(
        output,
        serde_json::json!({"status": "unknown", "idempotency_key": key})
    );
    assert_eq!(saved_send_key(&runtime, &alpha, &intent), Some(key));
}

#[test]
fn pending_send_keeps_its_key_after_the_wait() {
    let (_dir, runtime, alpha) = payment_runtime();
    let intent = send_intent(None);
    let (url, requests) = serve(vec![
        ("200 OK", activity_json("preparing", "", None)),
        (
            "200 OK",
            format!("[{}]", activity_json("broadcast", "t", None)),
        ),
    ]);

    let (result, output) = send_with(&runtime, &alpha, &url, Duration::from_millis(1200), &intent);

    assert_eq!(
        result.unwrap_err().to_string(),
        "payment is pending; run the same command to check it again"
    );
    let key = sent_key(&requests.recv().unwrap());
    assert_eq!(output["id"], "op");
    assert_eq!(output["status"], "broadcast");
    assert_eq!(output["idempotency_key"], key.as_str());
    assert_eq!(saved_send_key(&runtime, &alpha, &intent), Some(key));
}

#[test]
fn resumed_send_reports_a_stored_failure_and_clears_only_its_key() {
    let (_dir, runtime, alpha) = payment_runtime();
    let intent = send_intent(None);
    let journal = IntentJournal::open(&runtime.instance_dir(&alpha), "send").unwrap();
    let key = journal
        .key_for(&serde_json::to_string(&intent).unwrap())
        .unwrap();
    let other = journal.key_for("other").unwrap();
    drop(journal);
    let (url, requests) = serve(vec![(
        "200 OK",
        activity_json("failed", "t", Some("insufficient funds")),
    )]);

    let (result, output) = send_with(&runtime, &alpha, &url, Duration::from_secs(5), &intent);

    assert_eq!(
        result.unwrap_err().to_string(),
        "payment from account 1 to account 2 failed: insufficient funds"
    );
    assert_eq!(output["status"], "failed");
    assert_eq!(output["last_error"], "insufficient funds");
    assert_eq!(sent_key(&requests.recv().unwrap()), key);
    assert!(
        requests.try_recv().is_err(),
        "a resumed send must not create a payment"
    );
    assert_eq!(saved_send_key(&runtime, &alpha, &intent), None);
    let journal = IntentJournal::open(&runtime.instance_dir(&alpha), "send").unwrap();
    assert_eq!(journal.key_for("other").unwrap(), other);
}

#[test]
fn refused_send_is_not_polled_and_keeps_its_key() {
    let (_dir, runtime, alpha) = payment_runtime();
    let intent = send_intent(None);
    let (url, requests) = serve(vec![("400 Bad Request", "invalid memo".to_owned())]);

    let (result, output) = send_with(&runtime, &alpha, &url, Duration::from_secs(5), &intent);

    let error = result.unwrap_err();
    assert!(format!("{error:#}").contains("rejected (400 Bad Request): invalid memo"));
    assert_eq!(output, serde_json::Value::Null);
    let key = sent_key(&requests.recv().unwrap());
    assert!(requests.try_recv().is_err());
    assert_eq!(saved_send_key(&runtime, &alpha, &intent), Some(key));
}

#[test]
fn unresolved_json_adds_the_key_to_the_operation_fields() {
    let activity: Activity = serde_json::from_str(&activity_json("broadcast", "t", None)).unwrap();
    let pending = unresolved_json(Some(&activity), "key-1").unwrap();
    assert_eq!(pending["id"], "op");
    assert_eq!(pending["status"], "broadcast");
    assert_eq!(pending["txid"], "t");
    assert_eq!(pending["idempotency_key"], "key-1");
    let unknown = unresolved_json::<Activity>(None, "key-2").unwrap();
    assert_eq!(
        unknown,
        serde_json::json!({"status": "unknown", "idempotency_key": "key-2"})
    );
}

#[test]
fn address_faucet_reads_status_by_key_and_keeps_it_while_absent() {
    let (_dir, runtime, alpha) = payment_runtime();
    let intent = "address:utest:100";
    let pending = r#"{"address":"utest","amount_zatoshi":100,"txid":"","block_hash":null,"status":"preparing","last_error":null}"#;
    let (url, requests) = serve(vec![
        ("200 OK", pending),
        ("404 Not Found", "unknown idempotency key"),
    ]);

    let mut out = Vec::new();
    let error = runtime
        .faucet_with(
            &alpha,
            &url,
            &reqwest::blocking::Client::new(),
            &mut out,
            Instant::now() + Duration::from_millis(1200),
            "utest",
            100,
            true,
        )
        .unwrap_err();

    assert!(error.to_string().contains("pending"), "{error}");
    let key = sent_key(&requests.recv().unwrap());
    let output = json_stdout(&out);
    assert_eq!(output["address"], "utest");
    assert_eq!(output["status"], "preparing");
    assert_eq!(output["idempotency_key"], key.as_str());
    let (status_line, _) = requests.recv().unwrap();
    assert!(
        status_line.starts_with(&format!(
            "GET /api/v1/faucet/address?idempotency_key={key} "
        )),
        "{status_line}"
    );
    let journal = IntentJournal::open(&runtime.instance_dir(&alpha), "faucet").unwrap();
    assert_eq!(journal.key_for(intent).unwrap(), key);
}

#[test]
fn resumed_wallet_faucet_replays_completed_components_and_clears_once_all_settle() {
    let (_dir, runtime, alpha) = payment_runtime();
    let journal = IntentJournal::open(&runtime.instance_dir(&alpha), "faucet").unwrap();
    let batch = journal.wallet_batch("ironwood", 100, &[1, 2]).unwrap();
    let first = journal.key_for(&format!("{batch}:1")).unwrap();
    let second = journal.key_for(&format!("{batch}:2")).unwrap();
    drop(journal);
    let (url, requests) = serve(vec![
        ("200 OK", activity_json("confirmed", "t", None)),
        ("200 OK", activity_json("broadcast", "u", None)),
        (
            "200 OK",
            format!("[{}]", activity_json("failed", "u", Some("expired"))),
        ),
    ]);

    let mut out = Vec::new();
    let error = runtime
        .wallet_faucet_with(
            &alpha,
            &url,
            &reqwest::blocking::Client::new(),
            &mut out,
            Instant::now() + Duration::from_secs(5),
            &[1, 2],
            100,
            "ironwood",
            true,
        )
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "1 of 2 faucet requests are pending or failed"
    );
    let output = json_stdout(&out);
    assert_eq!(output["funded"].as_array().unwrap().len(), 1);
    assert_eq!(output["pending"], serde_json::json!([]));
    assert_eq!(
        output["failed"],
        serde_json::json!(["account 2: payment failed: expired"])
    );
    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(sent_key(&requests[0]), first);
    assert_eq!(sent_key(&requests[1]), second);
    assert!(requests[2].0.contains(&format!("idempotency_key={second}")));
    // Every component settled, so the whole batch is released for a new explicit invocation.
    let journal = IntentJournal::open(&runtime.instance_dir(&alpha), "faucet").unwrap();
    assert!(journal.read().unwrap().is_empty());
}

pub(super) fn send_interruption_probe() {
    let (Some(root), Some(url)) = (
        std::env::var_os("THS_SEND_PROBE_ROOT"),
        std::env::var("THS_SEND_PROBE_URL").ok(),
    ) else {
        return;
    };
    let runtime = Runtime { root: root.into() };
    let finished = send_with(
        &runtime,
        &name("alpha"),
        &url,
        Duration::from_secs(30),
        &send_intent(Some("")),
    );
    panic!("the interrupted send finished before it was killed: {finished:?}");
}
