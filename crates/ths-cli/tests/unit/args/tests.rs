use super::*;

#[test]
fn parses_explorer_commands_with_global_options() {
    for args in [
        vec!["ths", "--name", "alice", "--json", "tx", "txid"],
        vec!["ths", "tx", "txid", "--json", "--name", "alice"],
    ] {
        let cli = Cli::try_parse_from(args).unwrap();
        assert!(cli.json);
        assert_eq!(cli.name.unwrap().to_string(), "alice");
        assert!(matches!(cli.command, Some(Command::Tx { txid }) if txid == "txid"));
    }
    let cli = Cli::try_parse_from(["ths", "block"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Block { id: None })));
    for id in [
        "0",
        "100",
        "0000000000000000000000000000000000000000000000000000000000000000",
    ] {
        let cli = Cli::try_parse_from(["ths", "block", id, "--name", "alice", "--json"]).unwrap();
        assert!(cli.json);
        assert_eq!(cli.name.unwrap().to_string(), "alice");
        assert!(matches!(cli.command, Some(Command::Block { id: Some(value) }) if value == id));
    }
    assert!(Cli::try_parse_from(["ths", "tx"]).is_err());
    assert!(Cli::try_parse_from(["ths", "block", "1", "2"]).is_err());
}

#[test]
fn parses_reorg_and_rollback_depths() {
    for args in [
        vec!["ths", "reorg", "--depth", "3"],
        vec!["ths", "reorg", "--depth", "1000", "--keep-txs", "--json"],
        vec!["ths", "--name", "alice", "rollback", "--depth", "1"],
    ] {
        assert!(Cli::try_parse_from(&args).is_ok(), "{args:?}");
    }
    for command in ["reorg", "rollback"] {
        for depth in ["0", "1001", "-1", "1.5"] {
            assert!(Cli::try_parse_from(["ths", command, "--depth", depth]).is_err());
        }
        assert!(Cli::try_parse_from(["ths", command]).is_err());
    }
    assert!(Cli::try_parse_from(["ths", "rollback", "--depth", "1", "--keep-txs"]).is_err());
}

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
            detach: false,
            port_offset: 10,
            block_time: None,
            mine_on_mempool: false,
            no_auto_mine: false,
        })
    ));

    assert!(Cli::try_parse_from(["ths", "start", "--port-offset", "1"]).is_ok());

    for flag in ["--detach", "-d"] {
        let cli = Cli::try_parse_from(["ths", "start", flag]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Start {
                no_open: false,
                detach: true,
                port_offset: 0,
                ..
            })
        ));
    }
    assert!(Cli::try_parse_from(["ths", "start", "--detach", "--no-open"]).is_ok());

    let cli = Cli::try_parse_from(["ths", "endpoints"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Endpoints { env: false })
    ));

    let cli = Cli::try_parse_from(["ths", "--name", "alice", "endpoints", "--env"]).unwrap();
    assert_eq!(cli.name.unwrap().to_string(), "alice");
    assert!(matches!(
        cli.command,
        Some(Command::Endpoints { env: true })
    ));
}

#[test]
fn start_selects_one_mining_mode() {
    let cli = Cli::try_parse_from(["ths", "start", "--block-time", "2"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Start {
            block_time: Some(2),
            mine_on_mempool: false,
            no_auto_mine: false,
            ..
        })
    ));

    let cli = Cli::try_parse_from(["ths", "start", "--mine-on-mempool"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Start {
            block_time: None,
            mine_on_mempool: true,
            no_auto_mine: false,
            ..
        })
    ));

    let cli = Cli::try_parse_from(["ths", "start", "--no-auto-mine"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Start {
            block_time: None,
            mine_on_mempool: false,
            no_auto_mine: true,
            ..
        })
    ));

    // A zero interval cannot produce blocks, and the modes are exclusive.
    assert!(Cli::try_parse_from(["ths", "start", "--block-time", "0"]).is_err());
    assert!(
        Cli::try_parse_from(["ths", "start", "--block-time", "2", "--mine-on-mempool"]).is_err()
    );
    assert!(Cli::try_parse_from(["ths", "start", "--block-time", "2", "--no-auto-mine"]).is_err());
    assert!(Cli::try_parse_from(["ths", "start", "--mine-on-mempool", "--no-auto-mine"]).is_err());
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
fn wallet_quote_requires_an_account_and_both_pools() {
    let cli = Cli::try_parse_from([
        "ths",
        "--name",
        "demo",
        "--json",
        "wallet",
        "quote",
        "--from",
        "1",
        "--source-pool",
        "ironwood",
        "--destination-pool",
        "transparent",
    ])
    .unwrap();
    assert_eq!(cli.name.unwrap().to_string(), "demo");
    assert!(cli.json);
    assert!(matches!(
        cli.command,
        Some(Command::Wallet {
            action: WalletCommand::Quote {
                from: 1,
                source_pool: Pool::Ironwood,
                destination_pool: Pool::Transparent,
            }
        })
    ));

    let parse = |args: &[&str]| {
        let mut argv = vec!["ths", "wallet", "quote"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv)
    };
    let route = [
        "--source-pool",
        "transparent",
        "--destination-pool",
        "ironwood",
    ];
    assert!(parse(&[&["--from", "5"][..], &route].concat()).is_ok());
    assert!(parse(&route).is_err());
    assert!(parse(&["--from", "1", "--source-pool", "ironwood"]).is_err());
    assert!(parse(&["--from", "1", "--destination-pool", "ironwood"]).is_err());
    for from in ["0", "6"] {
        assert!(
            parse(&[&["--from", from][..], &route].concat()).is_err(),
            "accepted quote account {from}"
        );
    }
    assert!(
        parse(&[
            "--from",
            "1",
            "--source-pool",
            "orchard",
            "--destination-pool",
            "ironwood"
        ])
        .is_err()
    );
    for extra in [
        &["--amount", "1"][..],
        &["--max"],
        &["--to", "2"],
        &["--uri", "zcash:tm"],
    ] {
        assert!(
            parse(&[&["--from", "1"][..], &route, extra].concat()).is_err(),
            "accepted {extra:?}"
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
fn parses_rpc_method_params_and_flags() {
    let cli = Cli::try_parse_from([
        "ths",
        "--name",
        "alice",
        "rpc",
        "--raw",
        "getblockhash",
        "0",
    ])
    .unwrap();
    assert_eq!(cli.name.unwrap().to_string(), "alice");
    let Some(Command::Rpc {
        raw,
        yes,
        method,
        params,
    }) = cli.command
    else {
        panic!("expected rpc");
    };
    assert!(raw);
    assert!(!yes);
    assert_eq!(method, "getblockhash");
    assert_eq!(params, ["0"]);

    let cli = Cli::try_parse_from(["ths", "rpc", "getblockcount", "--raw"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Rpc {
            raw: true,
            yes: false,
            ..
        })
    ));

    let cli = Cli::try_parse_from(["ths", "rpc", "--yes", "stop"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Rpc {
            raw: false,
            yes: true,
            ..
        })
    ));

    let cli = Cli::try_parse_from(["ths", "rpc", "getblock", "-1"]).unwrap();
    let Some(Command::Rpc { params, .. }) = cli.command else {
        panic!("expected rpc");
    };
    assert_eq!(params, ["-1"]);

    assert!(Cli::try_parse_from(["ths", "rpc"]).is_err());
}

#[test]
fn send_to_address_excludes_other_destinations() {
    let address = "tmXzrDnkD3c38xn8CRzSqg8azgK24iuvzFZ";
    let send = |extra: &[&str]| {
        let mut args = vec![
            "ths",
            "wallet",
            "send",
            "--from",
            "2",
            "--to-address",
            address,
        ];
        args.extend_from_slice(extra);
        Cli::try_parse_from(args)
    };
    let cli = send(&["--amount", "0.1"]).unwrap();
    let Some(Command::Wallet {
        action: WalletCommand::Send(args),
    }) = cli.command
    else {
        panic!("expected wallet send");
    };
    assert_eq!(args.to_address.as_deref(), Some(address));
    assert_eq!(args.to, None);
    assert!(send(&["--max"]).is_ok());
    for conflicting in [
        &["--amount", "0.1", "--to", "3"][..],
        &["--amount", "0.1", "--uri", "zcash:tm"][..],
        &["--amount", "0.1", "--destination-pool", "ironwood"][..],
        &[][..],
    ] {
        assert!(send(conflicting).is_err(), "{conflicting:?}");
    }
}

#[test]
fn wallet_tx_takes_a_transaction_id_and_an_optional_account() {
    let txid = "AB".repeat(32);
    let cli = Cli::try_parse_from(["ths", "wallet", "tx", &txid, "--account", "2"]).unwrap();
    let Some(Command::Wallet {
        action: WalletCommand::Tx { txid, account },
    }) = cli.command
    else {
        panic!("expected wallet tx");
    };
    assert_eq!(txid, "ab".repeat(32));
    assert_eq!(account, Some(2));

    let txid = "ab".repeat(32);
    for args in [
        vec!["ths", "wallet", "tx"],
        vec!["ths", "wallet", "tx", "abc"],
        vec!["ths", "wallet", "tx", &"zz".repeat(32)],
        vec!["ths", "wallet", "tx", &txid, "--account", "6"],
        vec!["ths", "wallet", "tx", &txid, "--account", "0"],
    ] {
        assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
    }
}
