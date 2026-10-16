use super::*;

#[test]
fn separates_building_from_starting() {
    let default = Cli::try_parse_from(["ths"]).unwrap();
    assert!(default.command.is_none());

    let cli = Cli::try_parse_from(["ths", "build", "--dev"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Build { dev: true })));

    let cli = Cli::try_parse_from(["ths", "pull"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Pull)));

    let cli = Cli::try_parse_from(["ths", "update", "v1.2.3"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Update {
            version: Some(_),
            check: false
        })
    ));

    let cli = Cli::try_parse_from(["ths", "uninstall"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Uninstall)));

    let cli = Cli::try_parse_from(["ths", "mine", "10"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Mine { blocks: 10 })));

    let cli = Cli::try_parse_from(["ths", "--name", "alice", "mine", "3"]).unwrap();
    assert_eq!(cli.name.unwrap().to_string(), "alice");
    assert!(matches!(cli.command, Some(Command::Mine { blocks: 3 })));

    let cli = Cli::try_parse_from(["ths", "mine", "3", "--name", "alice"]).unwrap();
    assert_eq!(cli.name.unwrap().to_string(), "alice");

    assert!(Cli::try_parse_from(["ths", "mine", "0"]).is_err());
    assert!(Cli::try_parse_from(["ths", "mine", "10001"]).is_err());

    let cli = Cli::try_parse_from(["ths", "faucet", "uregtest1example"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Faucet {
            amount: ZecAmount(100_000_000),
            ..
        })
    ));

    let cli = Cli::try_parse_from([
        "ths",
        "faucet",
        "tmExample",
        "--amount",
        "1.25",
        "--name",
        "alice",
    ])
    .unwrap();
    assert_eq!(cli.name.unwrap().to_string(), "alice");
    assert!(matches!(
        cli.command,
        Some(Command::Faucet {
            amount: ZecAmount(125_000_000),
            ..
        })
    ));

    assert!(Cli::try_parse_from(["ths", "update", "1.2.3", "--check"]).is_err());

    assert!(Cli::try_parse_from(["ths", "start", "--build"]).is_err());

    let cli = Cli::try_parse_from(["ths", "start", "--port-offset", "10"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Start {
            no_open: false,
            port_offset: 10
        })
    ));

    assert!(Cli::try_parse_from(["ths", "start", "--port-offset", "1"]).is_ok());
}

#[test]
fn parses_log_line_selection() {
    let cli = Cli::try_parse_from(["ths", "logs"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Logs {
            service: None,
            follow: false,
            tail: None,
            head: None,
        })
    ));

    let cli = Cli::try_parse_from(["ths", "logs", "zakura", "--tail", "200", "-f"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Logs {
            service: Some(_),
            follow: true,
            tail: Some(200),
            head: None,
        })
    ));

    let cli = Cli::try_parse_from(["ths", "logs", "lightwalletd", "--head", "200"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Logs {
            service: Some(_),
            follow: false,
            tail: None,
            head: Some(200),
        })
    ));

    for invalid in [
        &["ths", "logs", "--tail", "0"][..],
        &["ths", "logs", "--head", "0"],
        &["ths", "logs", "--tail", "-1"],
        &["ths", "logs", "--head", "1.5"],
        &["ths", "logs", "--tail", "many"],
        &["ths", "logs", "--head", "5", "--tail", "5"],
        &["ths", "logs", "--head", "5", "--follow"],
    ] {
        assert!(
            Cli::try_parse_from(invalid).is_err(),
            "accepted {invalid:?}"
        );
    }
}

#[test]
fn wallet_send_and_shield_accept_bounded_memos() {
    let cli = Cli::try_parse_from([
        "ths",
        "wallet",
        "send",
        "--from",
        "1",
        "--to",
        "2",
        "--amount",
        "1.5",
        "--source-pool",
        "transparent",
        "--memo",
        "hello",
    ])
    .unwrap();
    let Some(Command::Wallet {
        action: WalletCommand::Send(args),
    }) = cli.command
    else {
        panic!("expected ths wallet send");
    };
    assert_eq!((args.from, args.to), (1, Some(2)));
    assert_eq!(args.amount.unwrap().zatoshi(), 150_000_000);
    assert_eq!(args.source_pool, Pool::Transparent);
    assert_eq!(args.destination_pool, Pool::Ironwood);
    assert_eq!(args.memo.as_deref(), Some("hello"));

    assert!(
        Cli::try_parse_from([
            "ths", "wallet", "send", "--from", "1", "--to", "6", "--amount", "1"
        ])
        .is_err()
    );

    // Account operations live only under `ths wallet`.
    for removed in ["send", "deploy"] {
        assert!(
            Cli::try_parse_from(["ths", removed, "--from", "1", "--to", "2", "--amount", "1"])
                .is_err(),
            "ths {removed} should not exist"
        );
    }

    let cli = Cli::try_parse_from([
        "ths", "wallet", "shield", "--from", "1", "--to", "2", "--amount", "1", "--memo", "hi",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Wallet {
            action: WalletCommand::Shield {
                to: 2,
                memo: Some(_),
                ..
            }
        })
    ));

    let too_long = "a".repeat(513);
    assert!(
        Cli::try_parse_from([
            "ths", "wallet", "send", "--from", "1", "--to", "2", "--amount", "1", "--memo",
            &too_long,
        ])
        .is_err()
    );
    assert!(
        Cli::try_parse_from([
            "ths", "wallet", "unshield", "--from", "1", "--to", "2", "--amount", "1", "--memo",
            "x",
        ])
        .is_err()
    );
    // Shield and unshield no longer default --to to --from.
    assert!(
        Cli::try_parse_from(["ths", "wallet", "shield", "--from", "1", "--amount", "1"]).is_err()
    );
}

#[test]
fn memo_values_follow_the_server_contract() {
    assert_eq!(parse_memo("").unwrap(), "");
    assert!(parse_memo(&"a".repeat(512)).is_ok());
    assert!(parse_memo(&"a".repeat(513)).is_err());
    assert!(parse_memo(&"🌸".repeat(128)).is_ok());
    assert!(parse_memo(&"🌸".repeat(129)).is_err());
    assert!(parse_memo("hi\0").is_err());
    assert!(parse_memo("a\0b").is_ok());
}

#[test]
fn parses_wallet_balances_with_global_json_after_subcommand() {
    let cli = Cli::try_parse_from(["ths", "wallet", "balances", "--json"]).unwrap();
    assert!(cli.json);
    assert!(matches!(
        cli.command,
        Some(Command::Wallet {
            action: WalletCommand::Balances
        })
    ));
}

#[test]
fn parses_wallet_activity_flags_and_rejects_invalid_limits() {
    let cli = Cli::try_parse_from(["ths", "wallet", "activity"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Wallet {
            action: WalletCommand::Activity { limit: None }
        })
    ));

    let cli =
        Cli::try_parse_from(["ths", "wallet", "activity", "--limit", "20", "--json"]).unwrap();
    assert!(cli.json);
    assert!(matches!(
        cli.command,
        Some(Command::Wallet {
            action: WalletCommand::Activity { limit: Some(20) }
        })
    ));

    for limit in ["0", "101"] {
        assert!(
            Cli::try_parse_from(["ths", "wallet", "activity", "--limit", limit]).is_err(),
            "accepted activity limit {limit}"
        );
    }
}

#[test]
fn wallet_send_takes_a_payment_uri_instead_of_a_destination() {
    let parse = |args: &[&str]| {
        let mut argv = vec!["ths", "wallet", "send", "--from", "1"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv)
    };
    let Ok(Cli {
        command: Some(Command::Wallet {
            action: WalletCommand::Send(args),
        }),
        ..
    }) = parse(&["--uri", "zcash:uregtest1abc?amount=1"])
    else {
        panic!("expected ths wallet send --uri");
    };
    assert_eq!(args.uri.as_deref(), Some("zcash:uregtest1abc?amount=1"));
    assert_eq!((args.to, args.amount), (None, None));
    assert!(!args.max);

    // --amount or --max can supply an amount omitted by the URI.
    assert!(parse(&["--uri", "zcash:uregtest1abc", "--amount", "1"]).is_ok());
    assert!(parse(&["--uri", "zcash:uregtest1abc", "--max"]).is_ok());
    assert!(parse(&["--uri", "zcash:uregtest1abc", "--amount", "1", "--max"]).is_err());
    // The URI already names the destination, its pool, and any memo.
    assert!(parse(&["--uri", "zcash:uregtest1abc", "--to", "2"]).is_err());
    assert!(
        parse(&[
            "--uri",
            "zcash:uregtest1abc",
            "--destination-pool",
            "ironwood"
        ])
        .is_err()
    );
    assert!(parse(&["--uri", "zcash:uregtest1abc", "--memo", "hi"]).is_err());
    // Without a URI, a destination and either --amount or --max are required.
    assert!(parse(&["--amount", "1"]).is_err());
    assert!(parse(&["--to", "2"]).is_err());
    assert!(parse(&["--to", "2", "--max"]).is_ok());
    assert!(parse(&["--to", "2", "--amount", "1", "--max"]).is_err());
}

#[test]
fn shield_and_unshield_accept_exactly_one_amount_mode() {
    for command in ["shield", "unshield"] {
        let parse = |amount: &[&str]| {
            let mut argv = vec!["ths", "wallet", command, "--from", "1", "--to", "1"];
            argv.extend_from_slice(amount);
            Cli::try_parse_from(argv)
        };

        assert!(parse(&["--amount", "1"]).is_ok());
        assert!(parse(&["--max"]).is_ok());
        assert!(parse(&[]).is_err());
        assert!(parse(&["--amount", "1", "--max"]).is_err());
    }
}

#[test]
fn completions_accept_only_bash_zsh_and_fish() {
    for (shell, expected) in [
        ("bash", CompletionShell::Bash),
        ("zsh", CompletionShell::Zsh),
        ("fish", CompletionShell::Fish),
    ] {
        let cli = Cli::try_parse_from(["ths", "completions", shell]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Completions { shell }) if shell == expected
        ));
    }

    let cli =
        Cli::try_parse_from(["ths", "--name", "alice", "--json", "completions", "zsh"]).unwrap();
    assert!(cli.json);
    assert_eq!(cli.name.unwrap().to_string(), "alice");
    assert!(matches!(
        cli.command,
        Some(Command::Completions {
            shell: CompletionShell::Zsh
        })
    ));

    assert!(Cli::try_parse_from(["ths", "completions"]).is_err());
    assert!(Cli::try_parse_from(["ths", "completions", "zsh", "bash"]).is_err());
    for shell in ["", " ", "powershell", "elvish", "ZSH", "Bash", "nu"] {
        assert!(
            Cli::try_parse_from(["ths", "completions", shell]).is_err(),
            "accepted shell {shell:?}"
        );
    }
}
