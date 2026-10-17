use super::*;
use clap::Parser;
use std::ffi::OsString;

#[test]
fn checks_for_updates_only_during_human_readable_startup() {
    let default = Cli::try_parse_from(["ths"]).unwrap();
    assert!(should_check_for_updates(&default));

    let start = Cli::try_parse_from(["ths", "start", "--no-open"]).unwrap();
    assert!(should_check_for_updates(&start));

    let json = Cli::try_parse_from(["ths", "--json"]).unwrap();
    assert!(!should_check_for_updates(&json));

    let status = Cli::try_parse_from(["ths", "status"]).unwrap();
    assert!(!should_check_for_updates(&status));
}

#[test]
fn parses_wait_and_run_without_reusing_the_default_name() {
    let run = Cli::try_parse_from(["ths", "run", "--", "npm", "test"]).unwrap();
    assert!(run.name.is_none());
    assert!(matches!(
        run.command,
        Some(Command::Run { timeout: 120, command })
            if command == [OsString::from("npm"), OsString::from("test")]
    ));

    let wait = Cli::try_parse_from(["ths", "--name", "api", "wait", "--timeout", "7"]).unwrap();
    assert_eq!(wait.name.as_ref().unwrap().to_string(), "api");
    assert!(matches!(wait.command, Some(Command::Wait { timeout: 7 })));

    assert!(Cli::try_parse_from(["ths", "--json", "run", "--", "true"]).is_ok());

    let first = generated_run_name();
    let second = generated_run_name();
    assert_ne!(first.to_string(), second.to_string());
    assert!(first.to_string().starts_with("run-"));

    assert_eq!(resolve_name(None, false).to_string(), "default");
}

#[test]
fn endpoints_env_rejects_json() {
    for args in [
        ["ths", "--json", "endpoints", "--env"],
        ["ths", "endpoints", "--env", "--json"],
    ] {
        let cli = Cli::try_parse_from(args).unwrap();
        let error = run(cli).unwrap_err().to_string();
        assert!(error.contains("endpoints --env"), "{error}");
    }
}
