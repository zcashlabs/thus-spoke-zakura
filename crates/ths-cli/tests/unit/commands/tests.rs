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
fn completion_scripts_cover_every_command_and_flag() {
    let scripts = [
        CompletionShell::Bash,
        CompletionShell::Zsh,
        CompletionShell::Fish,
    ]
    .map(|shell| {
        let mut buf = Vec::new();
        write_completions(shell, &mut buf);
        let script = String::from_utf8(buf).expect("completion script is utf-8");
        assert!(
            !script.trim().is_empty(),
            "{shell:?} produced an empty completion script"
        );
        assert_script_covers_cli(&script, shell);
        script
    });
    assert_ne!(scripts[0], scripts[1]);
    assert_ne!(scripts[1], scripts[2]);
    assert!(
        scripts[1].contains("#compdef ths"),
        "zsh script must register the ths command"
    );
    for value in [
        "bash",
        "zsh",
        "fish",
        "ironwood",
        "transparent",
        "app",
        "zakura",
        "lightwalletd",
    ] {
        assert!(
            scripts[1].contains(value),
            "zsh completion script is missing value {value}"
        );
    }
}

fn assert_script_covers_cli(script: &str, shell: CompletionShell) {
    let mut command = Cli::command();
    command.build();
    assert_script_covers(script, shell, &command, true);
}

fn assert_script_covers(script: &str, shell: CompletionShell, command: &clap::Command, root: bool) {
    if !root {
        let name = command.get_name();
        assert!(
            script_has_command(script, name),
            "{shell:?} completion script is missing command {name}"
        );
    }
    for arg in command.get_arguments() {
        if let Some(long) = arg.get_long() {
            assert!(
                script_has_long(script, shell, long),
                "{shell:?} completion script is missing --{long} on {}",
                command.get_name()
            );
        }
        if let Some(short) = arg.get_short() {
            assert!(
                script_has_short(script, shell, short),
                "{shell:?} completion script is missing -{short} on {}",
                command.get_name()
            );
        }
    }
    for subcommand in command.get_subcommands() {
        assert_script_covers(script, shell, subcommand, false);
    }
}

fn script_has_command(script: &str, name: &str) -> bool {
    script_words(script).any(|word| word == name)
}

fn script_has_long(script: &str, shell: CompletionShell, long: &str) -> bool {
    let needle = match shell {
        CompletionShell::Fish => format!("-l {long}"),
        CompletionShell::Bash | CompletionShell::Zsh => format!("--{long}"),
    };
    // `--to` must not match as a prefix of `--timeout`.
    script.match_indices(&needle).any(|(index, _)| {
        script[index + needle.len()..]
            .chars()
            .next()
            .is_none_or(|next| !next.is_ascii_alphanumeric() && next != '-')
    })
}

fn script_has_short(script: &str, shell: CompletionShell, short: char) -> bool {
    match shell {
        CompletionShell::Fish => script.contains(&format!("-s {short}")),
        CompletionShell::Zsh => {
            script.contains(&format!("'-{short}[")) || script.contains(&format!("'-{short}'"))
        }
        CompletionShell::Bash => {
            let flag = format!("-{short}");
            script_words(script).any(|word| word == flag)
        }
    }
}

fn script_words(script: &str) -> impl Iterator<Item = &str> {
    script
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
        .filter(|word| !word.is_empty())
}
