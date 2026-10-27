use super::*;

#[test]
fn faucet_keys_survive_cli_restart_until_the_whole_intent_is_confirmed() {
    let dir = std::env::temp_dir().join(format!("ths-faucet-journal-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let first = IntentJournal::open(&dir, "faucet").unwrap();
    let account_one = first.key_for("wallet:ironwood:100:1,2:1").unwrap();
    let account_two = first.key_for("wallet:ironwood:100:1,2:2").unwrap();
    drop(first);

    let retry = IntentJournal::open(&dir, "faucet").unwrap();
    assert_eq!(
        retry.key_for("wallet:ironwood:100:1,2:1").unwrap(),
        account_one
    );
    assert_eq!(
        retry.key_for("wallet:ironwood:100:1,2:2").unwrap(),
        account_two
    );
    retry
        .clear(&[
            ("wallet:ironwood:100:1,2:1", &account_one),
            ("wallet:ironwood:100:1,2:2", &account_two),
        ])
        .unwrap();
    drop(retry);

    let next = IntentJournal::open(&dir, "faucet").unwrap();
    assert_ne!(
        next.key_for("wallet:ironwood:100:1,2:1").unwrap(),
        account_one
    );
    drop(next);
    fs::remove_dir_all(dir).unwrap();
}

pub(super) fn overlapping_faucet_process_rejects_the_locked_journal() {
    let dir = tempfile::tempdir().unwrap();
    let journal = IntentJournal::open(dir.path(), "faucet").unwrap();
    let mut retry = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runtime::tests::faucet_journal_lock_probe"])
        .env("THS_FAUCET_JOURNAL_LOCK_PROBE", dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let completed_while_locked = loop {
        if retry.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(journal);
    let output = retry.wait_with_output().unwrap();
    let result = String::from_utf8_lossy(&output.stdout);
    assert!(
        completed_while_locked && output.status.success() && result.contains("running 1 test"),
        "overlapping command waited or acquired the lock: {result}"
    );
}

#[test]
fn unfinished_wallet_batch_rejects_an_overlapping_subset() {
    let dir = std::env::temp_dir().join(format!("ths-faucet-batch-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let journal = IntentJournal::open(&dir, "faucet").unwrap();
    let batch = journal.wallet_batch("ironwood", 100, &[1, 2]).unwrap();
    let first = journal.key_for(&format!("{batch}:1")).unwrap();
    assert!(journal.wallet_batch("ironwood", 100, &[2]).is_err());
    let second = journal.key_for(&format!("{batch}:2")).unwrap();

    assert_eq!(
        journal.wallet_batch("ironwood", 100, &[2, 1]).unwrap(),
        batch
    );
    assert!(journal.wallet_batch("ironwood", 100, &[2]).is_err());
    journal
        .clear(&[
            (&format!("{batch}:1"), &first),
            (&format!("{batch}:2"), &second),
        ])
        .unwrap();
    assert!(journal.wallet_batch("ironwood", 100, &[2]).is_ok());
    drop(journal);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[cfg(unix)]
#[cfg(unix)]
fn dropping_faucet_journal_releases_lock_with_a_duplicated_handle() {
    let dir = tempfile::tempdir().unwrap();
    let journal = IntentJournal::open(dir.path(), "faucet").unwrap();
    let key = journal.key_for("address:recipient:100").unwrap();
    // A concurrent process spawn can temporarily inherit the same open file
    // description. Keep a duplicate alive to reproduce that lifetime without timing.
    let duplicate = journal.lock.try_clone().unwrap();
    let contender = OpenOptions::new()
        .write(true)
        .open(dir.path().join("faucet-intents.lock"))
        .unwrap();
    assert!(matches!(
        contender.try_lock(),
        Err(TryLockError::WouldBlock)
    ));

    drop(journal);
    let retry = IntentJournal::open(dir.path(), "faucet").unwrap();
    assert_eq!(retry.key_for("address:recipient:100").unwrap(), key);
    assert!(matches!(
        contender.try_lock(),
        Err(TryLockError::WouldBlock)
    ));

    drop(duplicate);
    assert!(matches!(
        contender.try_lock(),
        Err(TryLockError::WouldBlock)
    ));
    drop(retry);
    contender.try_lock().unwrap();
}

#[test]
fn a_running_send_locks_its_journal_without_blocking_faucet_commands() {
    let dir = tempfile::tempdir().unwrap();
    let send = IntentJournal::open(dir.path(), "send").unwrap();
    let error = IntentJournal::open(dir.path(), "send").err().unwrap();
    assert!(
        error
            .to_string()
            .contains("another send command is running"),
        "{error}"
    );
    IntentJournal::open(dir.path(), "faucet").unwrap();
    drop(send);
    IntentJournal::open(dir.path(), "send").unwrap();
}

#[test]
fn a_late_result_cannot_clear_a_newer_key() {
    let dir = tempfile::tempdir().unwrap();
    let journal = IntentJournal::open(dir.path(), "send").unwrap();
    let newer = journal.key_for("intent").unwrap();
    journal.clear(&[("intent", "older-key")]).unwrap();
    assert_eq!(journal.key_for("intent").unwrap(), newer);
    journal.clear(&[("intent", &newer)]).unwrap();
    assert_ne!(journal.key_for("intent").unwrap(), newer);
}
