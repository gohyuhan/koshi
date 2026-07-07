//! Parse tests for the `koshi` command-line grammar: the bare interactive
//! launch, attach/detach root flags, lifecycle commands, the declared
//! subcommand tree, and usage-error diagnostics.

use clap::error::ErrorKind;
use clap::CommandFactory;
use clap::Parser;

use super::*;

fn parse(argv: &[&str]) -> Cli {
    Cli::try_parse_from(argv).expect("argv must parse")
}

fn parse_err(argv: &[&str]) -> clap::Error {
    Cli::try_parse_from(argv).expect_err("argv must fail to parse")
}

#[test]
fn bare_koshi_is_the_interactive_launch() {
    let cli = parse(&["koshi"]);
    assert_eq!(
        cli,
        Cli {
            attach: None,
            detach: None,
            command: None,
        }
    );
    assert!(cli.is_interactive_launch());
}

#[test]
fn attach_takes_a_required_session_id() {
    let cli = parse(&["koshi", "--attach", "3f2a"]);
    assert_eq!(
        cli,
        Cli {
            attach: Some("3f2a".to_string()),
            detach: None,
            command: None,
        }
    );
    assert!(!cli.is_interactive_launch());
}

#[test]
fn attach_without_a_session_id_is_a_usage_error() {
    let err = parse_err(&["koshi", "--attach"]);
    assert_eq!(err.kind(), ErrorKind::InvalidValue);
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn detach_without_an_id_targets_the_current_session() {
    let cli = parse(&["koshi", "--detach"]);
    assert_eq!(
        cli,
        Cli {
            attach: None,
            detach: Some(None),
            command: None,
        }
    );
    assert!(!cli.is_interactive_launch());
}

#[test]
fn detach_with_an_id_targets_that_session() {
    let cli = parse(&["koshi", "--detach", "3f2a"]);
    assert_eq!(
        cli,
        Cli {
            attach: None,
            detach: Some(Some("3f2a".to_string())),
            command: None,
        }
    );
}

#[test]
fn detach_binds_a_subcommand_looking_token_as_its_value() {
    let cli = parse(&["koshi", "--detach", "list-sessions"]);
    assert_eq!(
        cli,
        Cli {
            attach: None,
            detach: Some(Some("list-sessions".to_string())),
            command: None,
        }
    );
}

#[test]
fn attach_and_detach_conflict() {
    let err = parse_err(&["koshi", "--attach", "3f2a", "--detach"]);
    assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn attach_conflicts_with_subcommands() {
    let err = parse_err(&["koshi", "--attach", "3f2a", "list-sessions"]);
    assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
}

#[test]
fn lifecycle_commands_parse() {
    assert_eq!(parse(&["koshi", "new"]).command, Some(CliCommand::New));
    assert_eq!(
        parse(&["koshi", "list-sessions"]).command,
        Some(CliCommand::ListSessions)
    );
    assert_eq!(
        parse(&["koshi", "doctor"]).command,
        Some(CliCommand::Doctor)
    );
}

#[test]
fn a_subcommand_is_not_the_interactive_launch() {
    assert!(!parse(&["koshi", "new"]).is_interactive_launch());
}

#[test]
fn kill_session_takes_an_optional_session() {
    assert_eq!(
        parse(&["koshi", "kill-session"]).command,
        Some(CliCommand::KillSession { session: None })
    );
    assert_eq!(
        parse(&["koshi", "kill-session", "work"]).command,
        Some(CliCommand::KillSession {
            session: Some("work".to_string())
        })
    );
}

#[test]
fn kill_session_rejects_a_second_positional() {
    let err = parse_err(&["koshi", "kill-session", "work", "extra"]);
    assert_eq!(err.kind(), ErrorKind::UnknownArgument);
}

#[test]
fn declared_subcommands_parse_to_their_variants() {
    let cases: &[(&str, CliCommand)] = &[
        ("action", CliCommand::Action),
        ("new-pane", CliCommand::NewPane),
        ("close-pane", CliCommand::ClosePane),
        ("resize-pane", CliCommand::ResizePane),
        ("toggle-pane-fullscreen", CliCommand::TogglePaneFullscreen),
        ("rename-pane", CliCommand::RenamePane),
        ("new-tab", CliCommand::NewTab),
        ("close-tab", CliCommand::CloseTab),
        ("next-tab", CliCommand::NextTab),
        ("previous-tab", CliCommand::PreviousTab),
        ("rename-tab", CliCommand::RenameTab),
        ("move-tab", CliCommand::MoveTab),
        ("focus-tab", CliCommand::FocusTab),
        ("focus-pane", CliCommand::FocusPane),
        ("lock", CliCommand::Lock),
        ("unlock", CliCommand::Unlock),
        ("toggle-lock", CliCommand::ToggleLock),
        ("config", CliCommand::Config),
        ("plugin", CliCommand::Plugin),
        ("rename-session", CliCommand::RenameSession),
        ("actions", CliCommand::Actions),
        ("inspect", CliCommand::Inspect),
        ("list-tabs", CliCommand::ListTabs),
        ("list-panes", CliCommand::ListPanes),
        ("list-clients", CliCommand::ListClients),
        ("run", CliCommand::Run),
        ("keys", CliCommand::Keys),
    ];
    for (name, expected) in cases {
        assert_eq!(parse(&["koshi", name]).command.as_ref(), Some(expected));
    }
}

#[test]
fn the_command_tree_lists_exactly_the_declared_subcommands() {
    let mut names: Vec<String> = Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect();
    names.sort();
    let mut expected: Vec<String> = [
        "action",
        "actions",
        "close-pane",
        "close-tab",
        "config",
        "doctor",
        "focus-pane",
        "focus-tab",
        "inspect",
        "keys",
        "kill-session",
        "list-clients",
        "list-panes",
        "list-sessions",
        "list-tabs",
        "lock",
        "move-tab",
        "new",
        "new-pane",
        "new-tab",
        "next-tab",
        "plugin",
        "previous-tab",
        "rename-pane",
        "rename-session",
        "rename-tab",
        "resize-pane",
        "run",
        "toggle-lock",
        "toggle-pane-fullscreen",
        "unlock",
    ]
    .map(String::from)
    .to_vec();
    expected.sort();
    assert_eq!(names, expected);
}

#[test]
fn declared_subcommands_reject_arguments() {
    let err = parse_err(&["koshi", "new-pane", "--direction", "right"]);
    assert_eq!(err.kind(), ErrorKind::UnknownArgument);
}

#[test]
fn an_unknown_subcommand_is_a_usage_error() {
    let err = parse_err(&["koshi", "explode"]);
    assert_eq!(err.kind(), ErrorKind::InvalidSubcommand);
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn an_unknown_flag_is_a_usage_error() {
    let err = parse_err(&["koshi", "--frobnicate"]);
    assert_eq!(err.kind(), ErrorKind::UnknownArgument);
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn help_and_version_display_and_exit_zero() {
    let help = parse_err(&["koshi", "--help"]);
    assert_eq!(help.kind(), ErrorKind::DisplayHelp);
    assert_eq!(help.exit_code(), 0);

    let version = parse_err(&["koshi", "--version"]);
    assert_eq!(version.kind(), ErrorKind::DisplayVersion);
    assert_eq!(version.exit_code(), 0);
}

#[test]
fn every_subcommand_answers_help() {
    for name in Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect::<Vec<_>>()
    {
        let err = parse_err(&["koshi", &name, "--help"]);
        assert_eq!(err.kind(), ErrorKind::DisplayHelp, "for subcommand {name}");
    }
}
