//! Parse tests for the `koshi` command-line grammar: the bare interactive
//! launch, attach/detach root flags, lifecycle commands, the typed action
//! subcommands and their command mapping, and usage-error diagnostics.

use clap::error::ErrorKind;
use clap::CommandFactory;
use clap::Parser;
use koshi_core::action::{core_action_seeds, ActionHandlerRef};

use super::*;

fn parse(argv: &[&str]) -> Cli {
    Cli::try_parse_from(argv).expect("argv must parse")
}

fn parse_err(argv: &[&str]) -> clap::Error {
    Cli::try_parse_from(argv).expect_err("argv must fail to parse")
}

/// The parsed subcommand of `argv`.
fn command(argv: &[&str]) -> CliCommand {
    parse(argv).command.expect("argv must carry a subcommand")
}

/// The `(action, command)` pair the subcommand of `argv` maps to.
fn action_of(argv: &[&str]) -> (ActionRef, Command) {
    command(argv)
        .to_action()
        .expect("argv must map to an action")
}

/// A fixed UUID so id-carrying asserts are exact.
fn fixed_uuid() -> Uuid {
    Uuid::parse_str("0192f0c1-2345-7000-8000-000000000001").expect("literal UUID is valid")
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
        Some(CliCommand::ListSessions {
            format: FormatArg::Table
        })
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
fn flagless_subcommands_parse_to_their_variants() {
    let cases: &[(&str, CliCommand)] = &[
        ("toggle-pane-fullscreen", CliCommand::TogglePaneFullscreen),
        ("new-tab", CliCommand::NewTab),
        ("next-tab", CliCommand::NextTab { client: None }),
        ("previous-tab", CliCommand::PreviousTab { client: None }),
        ("lock", CliCommand::Lock),
        ("unlock", CliCommand::Unlock),
        ("toggle-lock", CliCommand::ToggleLock),
        ("config", CliCommand::Config),
        ("plugin", CliCommand::Plugin),
        ("actions", CliCommand::Actions),
        (
            "list-tabs",
            CliCommand::ListTabs {
                session: None,
                format: FormatArg::Table,
            },
        ),
        (
            "list-panes",
            CliCommand::ListPanes {
                session: None,
                tab: None,
                format: FormatArg::Table,
            },
        ),
        (
            "list-clients",
            CliCommand::ListClients {
                session: None,
                format: FormatArg::Table,
            },
        ),
        ("keys", CliCommand::Keys),
    ];
    for (name, expected) in cases {
        assert_eq!(parse(&["koshi", name]).command.as_ref(), Some(expected));
    }
}

// --- Discovery queries ---

#[test]
fn list_tabs_parses_a_typed_session_and_a_format() {
    let session = format!("session-{}", fixed_uuid());
    assert_eq!(
        parse(&[
            "koshi",
            "list-tabs",
            "--session",
            &session,
            "--format",
            "json"
        ])
        .command,
        Some(CliCommand::ListTabs {
            session: Some(SessionId::from_uuid(fixed_uuid())),
            format: FormatArg::Json,
        })
    );
}

#[test]
fn list_panes_parses_a_tab_filter() {
    let tab = format!("tab-{}", fixed_uuid());
    assert_eq!(
        parse(&["koshi", "list-panes", "--tab", &tab]).command,
        Some(CliCommand::ListPanes {
            session: None,
            tab: Some(TabId::from_uuid(fixed_uuid())),
            format: FormatArg::Table,
        })
    );
}

#[test]
fn list_sessions_parses_the_json_format() {
    assert_eq!(
        parse(&["koshi", "list-sessions", "--format", "json"]).command,
        Some(CliCommand::ListSessions {
            format: FormatArg::Json,
        })
    );
}

#[test]
fn format_rejects_an_unknown_value() {
    let err = parse_err(&["koshi", "list-sessions", "--format", "yaml"]);
    assert_eq!(err.kind(), ErrorKind::InvalidValue);
}

#[test]
fn inspect_forms_parse_typed_ids() {
    let uuid = fixed_uuid();
    let cases: &[(&str, String, InspectTarget)] = &[
        (
            "session",
            format!("session-{uuid}"),
            InspectTarget::Session {
                session: SessionId::from_uuid(uuid),
                format: FormatArg::Table,
            },
        ),
        (
            "tab",
            format!("tab-{uuid}"),
            InspectTarget::Tab {
                tab: TabId::from_uuid(uuid),
                format: FormatArg::Table,
            },
        ),
        (
            "pane",
            format!("pane-{uuid}"),
            InspectTarget::Pane {
                pane: PaneId::from_uuid(uuid),
                format: FormatArg::Table,
            },
        ),
        (
            "client",
            format!("client-{uuid}"),
            InspectTarget::Client {
                client: ClientId::from_uuid(uuid),
                format: FormatArg::Table,
            },
        ),
    ];
    for (kind, id, expected) in cases {
        let command = command(&["koshi", "inspect", kind, id]);
        let CliCommand::Inspect { target } = command else {
            panic!("expected an inspect command for {kind}, got {command:?}");
        };
        assert_eq!(&target, expected, "for {kind}");
    }
}

#[test]
fn inspect_parses_the_json_format() {
    let pane = format!("pane-{}", fixed_uuid());
    assert_eq!(
        parse(&["koshi", "inspect", "pane", &pane, "--format", "json"]).command,
        Some(CliCommand::Inspect {
            target: InspectTarget::Pane {
                pane: PaneId::from_uuid(fixed_uuid()),
                format: FormatArg::Json,
            }
        })
    );
}

#[test]
fn inspect_requires_a_target() {
    let err = parse_err(&["koshi", "inspect"]);
    assert_eq!(
        err.kind(),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn inspect_rejects_an_id_of_the_wrong_kind() {
    let tab_id = format!("tab-{}", fixed_uuid());
    let err = parse_err(&["koshi", "inspect", "pane", &tab_id]);
    assert_eq!(err.kind(), ErrorKind::ValueValidation);
}

#[test]
fn the_command_tree_lists_exactly_the_declared_subcommands() {
    let mut names: Vec<String> = Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect();
    names.sort();
    let mut expected: Vec<String> = [
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

// --- Typed action arguments ---

#[test]
fn new_pane_parses_bare_and_with_every_flag() {
    assert_eq!(
        command(&["koshi", "new-pane"]),
        CliCommand::NewPane {
            direction: None,
            stacked: false,
            pane: None,
        }
    );
    let pane_flag = format!("pane-{}", fixed_uuid());
    assert_eq!(
        command(&[
            "koshi",
            "new-pane",
            "--direction",
            "right",
            "--pane",
            &pane_flag
        ]),
        CliCommand::NewPane {
            direction: Some(DirectionArg::Right),
            stacked: false,
            pane: Some(PaneId::from_uuid(fixed_uuid())),
        }
    );
    assert_eq!(
        command(&["koshi", "new-pane", "--stacked"]),
        CliCommand::NewPane {
            direction: None,
            stacked: true,
            pane: None,
        }
    );
}

#[test]
fn new_pane_direction_and_stacked_conflict() {
    let err = parse_err(&["koshi", "new-pane", "--direction", "left", "--stacked"]);
    assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn an_invalid_direction_is_a_usage_error() {
    let err = parse_err(&["koshi", "new-pane", "--direction", "sideways"]);
    assert_eq!(err.kind(), ErrorKind::InvalidValue);
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn every_direction_value_parses_to_its_core_direction() {
    let cases: &[(&str, Direction)] = &[
        ("right", Direction::Right),
        ("down", Direction::Down),
        ("left", Direction::Left),
        ("up", Direction::Up),
    ];
    for (value, expected) in cases {
        let (_, mapped) = action_of(&["koshi", "new-pane", "--direction", value]);
        assert_eq!(
            mapped,
            Command::NewPane(NewPaneArgs {
                source: None,
                direction: Some(*expected),
                stacked: false,
                cwd: None,
                command: None,
                client: None,
            })
        );
    }
}

#[test]
fn close_pane_parses_target_and_force() {
    assert_eq!(
        command(&["koshi", "close-pane"]),
        CliCommand::ClosePane {
            pane: None,
            force: false,
        }
    );
    let pane_flag = format!("pane-{}", fixed_uuid());
    assert_eq!(
        command(&["koshi", "close-pane", "--pane", &pane_flag, "--force"]),
        CliCommand::ClosePane {
            pane: Some(PaneId::from_uuid(fixed_uuid())),
            force: true,
        }
    );
}

#[test]
fn resize_pane_defaults_the_size_to_one() {
    assert_eq!(
        command(&["koshi", "resize-pane", "--direction", "left"]),
        CliCommand::ResizePane {
            direction: DirectionArg::Left,
            size: 1,
            pane: None,
        }
    );
}

#[test]
fn resize_pane_accepts_a_negative_size_in_both_spellings() {
    assert_eq!(
        command(&["koshi", "resize-pane", "--direction", "up", "--size", "-3"]),
        CliCommand::ResizePane {
            direction: DirectionArg::Up,
            size: -3,
            pane: None,
        }
    );
    assert_eq!(
        command(&["koshi", "resize-pane", "--direction", "up", "--size=-3"]),
        CliCommand::ResizePane {
            direction: DirectionArg::Up,
            size: -3,
            pane: None,
        }
    );
}

#[test]
fn resize_pane_requires_a_direction() {
    let err = parse_err(&["koshi", "resize-pane", "--size", "2"]);
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn rename_pane_and_rename_tab_take_optional_targets() {
    assert_eq!(
        command(&["koshi", "rename-pane"]),
        CliCommand::RenamePane { pane: None }
    );
    let pane_flag = format!("pane-{}", fixed_uuid());
    assert_eq!(
        command(&["koshi", "rename-pane", "--pane", &pane_flag]),
        CliCommand::RenamePane {
            pane: Some(PaneId::from_uuid(fixed_uuid())),
        }
    );
    let tab_flag = format!("tab-{}", fixed_uuid());
    assert_eq!(
        command(&["koshi", "rename-tab", "--tab", &tab_flag]),
        CliCommand::RenameTab {
            tab: Some(TabId::from_uuid(fixed_uuid())),
        }
    );
}

#[test]
fn close_tab_parses_target_and_force() {
    let tab_flag = format!("tab-{}", fixed_uuid());
    assert_eq!(
        command(&["koshi", "close-tab", "--tab", &tab_flag, "--force"]),
        CliCommand::CloseTab {
            tab: Some(TabId::from_uuid(fixed_uuid())),
            force: true,
        }
    );
}

#[test]
fn move_tab_requires_an_index() {
    assert_eq!(
        command(&["koshi", "move-tab", "--index", "2"]),
        CliCommand::MoveTab {
            index: 2,
            tab: None,
        }
    );
    let err = parse_err(&["koshi", "move-tab"]);
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn focus_tab_takes_exactly_one_of_index_or_tab() {
    assert_eq!(
        command(&["koshi", "focus-tab", "--index", "1"]),
        CliCommand::FocusTab {
            index: Some(1),
            tab: None,
            client: None,
        }
    );
    let tab_flag = format!("tab-{}", fixed_uuid());
    assert_eq!(
        command(&["koshi", "focus-tab", "--tab", &tab_flag]),
        CliCommand::FocusTab {
            index: None,
            tab: Some(TabId::from_uuid(fixed_uuid())),
            client: None,
        }
    );

    let both = parse_err(&["koshi", "focus-tab", "--index", "1", "--tab", &tab_flag]);
    assert_eq!(both.kind(), ErrorKind::ArgumentConflict);
    let neither = parse_err(&["koshi", "focus-tab"]);
    assert_eq!(neither.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn tab_focus_commands_take_an_optional_client() {
    let client_flag = format!("client-{}", fixed_uuid());
    let client = ClientId::from_uuid(fixed_uuid());
    assert_eq!(
        command(&[
            "koshi",
            "focus-tab",
            "--index",
            "1",
            "--client",
            &client_flag
        ]),
        CliCommand::FocusTab {
            index: Some(1),
            tab: None,
            client: Some(client),
        }
    );
    assert_eq!(
        command(&["koshi", "next-tab", "--client", &client_flag]),
        CliCommand::NextTab {
            client: Some(client),
        }
    );
    assert_eq!(
        command(&["koshi", "previous-tab", "--client", &client_flag]),
        CliCommand::PreviousTab {
            client: Some(client),
        }
    );

    // The client rides into the mapped command for all three verbs.
    let (_, mapped) = action_of(&["koshi", "next-tab", "--client", &client_flag]);
    assert_eq!(
        mapped,
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Next,
            client: Some(client),
        })
    );
    let (_, mapped) = action_of(&[
        "koshi",
        "focus-tab",
        "--tab",
        &format!("tab-{}", fixed_uuid()),
        "--client",
        &client_flag,
    ]);
    assert_eq!(
        mapped,
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(TabId::from_uuid(fixed_uuid())),
            client: Some(client),
        })
    );
}

#[test]
fn focus_pane_requires_a_pane_and_takes_an_optional_client() {
    let err = parse_err(&["koshi", "focus-pane"]);
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);

    let pane_flag = format!("pane-{}", fixed_uuid());
    let client_flag = format!("client-{}", fixed_uuid());
    assert_eq!(
        command(&[
            "koshi",
            "focus-pane",
            "--pane",
            &pane_flag,
            "--client",
            &client_flag
        ]),
        CliCommand::FocusPane {
            pane: PaneId::from_uuid(fixed_uuid()),
            client: Some(ClientId::from_uuid(fixed_uuid())),
        }
    );
}

#[test]
fn rename_session_takes_an_optional_session_id() {
    assert_eq!(
        command(&["koshi", "rename-session"]),
        CliCommand::RenameSession { session: None }
    );
    let session_flag = format!("session-{}", fixed_uuid());
    assert_eq!(
        command(&["koshi", "rename-session", "--session", &session_flag]),
        CliCommand::RenameSession {
            session: Some(SessionId::from_uuid(fixed_uuid())),
        }
    );
}

#[test]
fn run_takes_its_command_after_the_separator() {
    assert_eq!(
        command(&["koshi", "run", "--", "htop", "-d", "5"]),
        CliCommand::Run {
            direction: None,
            stacked: false,
            pane: None,
            command: vec!["htop".to_string(), "-d".to_string(), "5".to_string()],
        }
    );
    assert_eq!(
        command(&["koshi", "run", "--direction", "down", "--", "htop"]),
        CliCommand::Run {
            direction: Some(DirectionArg::Down),
            stacked: false,
            pane: None,
            command: vec!["htop".to_string()],
        }
    );
}

#[test]
fn run_takes_an_optional_source_pane() {
    let pane_flag = format!("pane-{}", fixed_uuid());
    assert_eq!(
        command(&["koshi", "run", "--pane", &pane_flag, "--", "htop"]),
        CliCommand::Run {
            direction: None,
            stacked: false,
            pane: Some(PaneId::from_uuid(fixed_uuid())),
            command: vec!["htop".to_string()],
        }
    );

    // The source pane rides into the mapped command.
    let (_, mapped) = action_of(&["koshi", "run", "--pane", &pane_flag, "--", "htop"]);
    assert_eq!(
        mapped,
        Command::RunCommandPane(RunCommandPaneArgs {
            command: SpawnSpec {
                program: PathBuf::from("htop"),
                args: vec![],
                cwd: None,
                env: BTreeMap::new(),
                shell_kind: ShellKind::Other("htop".to_string()),
            },
            cwd: None,
            source: Some(PaneId::from_uuid(fixed_uuid())),
            direction: None,
            stacked: false,
        })
    );
}

#[test]
fn run_without_a_command_is_a_usage_error() {
    let bare = parse_err(&["koshi", "run"]);
    assert_eq!(bare.kind(), ErrorKind::MissingRequiredArgument);
    let empty = parse_err(&["koshi", "run", "--"]);
    assert_eq!(empty.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn run_rejects_a_command_not_behind_the_separator() {
    let err = parse_err(&["koshi", "run", "htop"]);
    assert_eq!(err.kind(), ErrorKind::UnknownArgument);
}

#[test]
fn run_direction_and_stacked_conflict() {
    let err = parse_err(&[
        "koshi",
        "run",
        "--direction",
        "up",
        "--stacked",
        "--",
        "htop",
    ]);
    assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
}

// --- Id parsing ---

#[test]
fn ids_parse_prefixed_and_bare_forms() {
    let bare = fixed_uuid().to_string();
    assert_eq!(
        command(&["koshi", "close-pane", "--pane", &bare]),
        CliCommand::ClosePane {
            pane: Some(PaneId::from_uuid(fixed_uuid())),
            force: false,
        }
    );
    let prefixed = format!("pane-{}", fixed_uuid());
    assert_eq!(
        command(&["koshi", "close-pane", "--pane", &prefixed]),
        CliCommand::ClosePane {
            pane: Some(PaneId::from_uuid(fixed_uuid())),
            force: false,
        }
    );
}

#[test]
fn an_id_of_the_wrong_kind_is_a_usage_error() {
    let tab_id = format!("tab-{}", fixed_uuid());
    let err = parse_err(&["koshi", "close-pane", "--pane", &tab_id]);
    assert_eq!(err.kind(), ErrorKind::ValueValidation);
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn a_malformed_id_is_a_usage_error() {
    let err = parse_err(&["koshi", "close-pane", "--pane", "not-a-uuid"]);
    assert_eq!(err.kind(), ErrorKind::ValueValidation);
    assert_eq!(err.exit_code(), 2);
}

// --- Action mapping ---

#[test]
fn action_subcommands_map_to_their_exact_commands() {
    let pane = PaneId::from_uuid(fixed_uuid());
    let pane_flag = format!("pane-{}", fixed_uuid());
    let tab = TabId::from_uuid(fixed_uuid());
    let tab_flag = format!("tab-{}", fixed_uuid());
    let session = SessionId::from_uuid(fixed_uuid());
    let session_flag = format!("session-{}", fixed_uuid());

    let cases: Vec<(Vec<&str>, &str, Command)> = vec![
        (
            vec!["koshi", "new-pane", "--direction", "right"],
            "new-pane",
            Command::NewPane(NewPaneArgs {
                source: None,
                direction: Some(Direction::Right),
                stacked: false,
                cwd: None,
                command: None,
                client: None,
            }),
        ),
        (
            vec!["koshi", "new-pane", "--stacked", "--pane", &pane_flag],
            "new-pane",
            Command::NewPane(NewPaneArgs {
                source: Some(pane),
                direction: None,
                stacked: true,
                cwd: None,
                command: None,
                client: None,
            }),
        ),
        (
            vec!["koshi", "close-pane", "--force"],
            "close-pane",
            Command::ClosePane(ClosePaneArgs {
                pane: None,
                force: true,
            }),
        ),
        (
            vec![
                "koshi",
                "resize-pane",
                "--direction",
                "left",
                "--size",
                "-5",
            ],
            "resize-pane",
            Command::ResizePane(ResizePaneArgs {
                pane: None,
                direction: Direction::Left,
                size: -5,
            }),
        ),
        (
            vec!["koshi", "toggle-pane-fullscreen"],
            "toggle-pane-fullscreen",
            Command::TogglePaneFullscreen,
        ),
        (
            vec!["koshi", "rename-pane", "--pane", &pane_flag],
            "rename-pane",
            Command::RenamePane(RenamePaneArgs { pane: Some(pane) }),
        ),
        (
            vec!["koshi", "new-tab"],
            "new-tab",
            Command::NewTab(NewTabArgs {
                cwd: None,
                client: None,
            }),
        ),
        (
            vec!["koshi", "close-tab", "--tab", &tab_flag],
            "close-tab",
            Command::CloseTab(CloseTabArgs {
                tab: Some(tab),
                force: false,
            }),
        ),
        (
            vec!["koshi", "next-tab"],
            "next-tab",
            Command::FocusTab(FocusTabArgs {
                target: TabTarget::Next,
                client: None,
            }),
        ),
        (
            vec!["koshi", "previous-tab"],
            "previous-tab",
            Command::FocusTab(FocusTabArgs {
                target: TabTarget::Prev,
                client: None,
            }),
        ),
        (
            vec!["koshi", "rename-tab"],
            "rename-tab",
            Command::RenameTab(RenameTabArgs { tab: None }),
        ),
        (
            vec!["koshi", "move-tab", "--index", "3", "--tab", &tab_flag],
            "move-tab",
            Command::MoveTab(MoveTabArgs {
                tab: Some(tab),
                index: 3,
            }),
        ),
        (
            vec!["koshi", "focus-tab", "--index", "0"],
            "focus-tab",
            Command::FocusTab(FocusTabArgs {
                target: TabTarget::Index(0),
                client: None,
            }),
        ),
        (
            vec!["koshi", "focus-tab", "--tab", &tab_flag],
            "focus-tab",
            Command::FocusTab(FocusTabArgs {
                target: TabTarget::Id(tab),
                client: None,
            }),
        ),
        (
            vec!["koshi", "focus-pane", "--pane", &pane_flag],
            "focus-pane",
            Command::FocusPane(FocusPaneArgs { pane, client: None }),
        ),
        (
            vec!["koshi", "lock"],
            "lock",
            Command::SetLockMode(LockModeArgs { locked: true }),
        ),
        (
            vec!["koshi", "unlock"],
            "unlock",
            Command::SetLockMode(LockModeArgs { locked: false }),
        ),
        (
            vec!["koshi", "toggle-lock"],
            "toggle-lock",
            Command::ToggleLockMode,
        ),
        (
            vec!["koshi", "rename-session", "--session", &session_flag],
            "rename-session",
            Command::RenameSession(RenameSessionArgs {
                session: Some(session),
            }),
        ),
        (
            vec!["koshi", "run", "--stacked", "--", "htop", "-d", "5"],
            "run",
            Command::RunCommandPane(RunCommandPaneArgs {
                command: SpawnSpec {
                    program: PathBuf::from("htop"),
                    args: vec!["-d".to_string(), "5".to_string()],
                    cwd: None,
                    env: BTreeMap::new(),
                    shell_kind: ShellKind::Other("htop".to_string()),
                },
                cwd: None,
                source: None,
                direction: None,
                stacked: true,
            }),
        ),
    ];

    for (argv, name, expected) in cases {
        let (action, mapped) = action_of(&argv);
        assert_eq!(
            action,
            ActionRef::core(name).expect("test action names are valid"),
            "for {argv:?}"
        );
        assert_eq!(mapped, expected, "for {argv:?}");
    }
}

#[test]
fn every_mapped_action_matches_its_seeded_command_kind() {
    // Each argv below exercises one CLI action surface; its mapping must
    // agree with the seed table on both the action's existence and the
    // command it dispatches, so the two surfaces cannot drift apart.
    let seeds = core_action_seeds();
    let argvs: &[&[&str]] = &[
        &["koshi", "new-pane"],
        &["koshi", "close-pane"],
        &["koshi", "resize-pane", "--direction", "left"],
        &["koshi", "toggle-pane-fullscreen"],
        &["koshi", "rename-pane"],
        &["koshi", "new-tab"],
        &["koshi", "close-tab"],
        &["koshi", "next-tab"],
        &["koshi", "previous-tab"],
        &["koshi", "rename-tab"],
        &["koshi", "move-tab", "--index", "0"],
        &["koshi", "focus-tab", "--index", "0"],
        &[
            "koshi",
            "focus-pane",
            "--pane",
            "0192f0c1-2345-7000-8000-000000000001",
        ],
        &["koshi", "lock"],
        &["koshi", "unlock"],
        &["koshi", "toggle-lock"],
        &["koshi", "rename-session"],
        &["koshi", "run", "--", "htop"],
    ];

    for argv in argvs {
        let (action, mapped) = action_of(argv);
        let (_, metadata) = seeds
            .iter()
            .find(|(seeded, _)| *seeded == action)
            .unwrap_or_else(|| panic!("action {action} is not in the seed table"));
        let ActionHandlerRef::CoreCommand(kind) = &metadata.handler else {
            panic!("action {action} is seeded with a non-core handler");
        };
        assert_eq!(mapped.kind(), *kind, "for {argv:?}");
    }
}

#[test]
fn non_action_subcommands_map_to_none() {
    let argvs: &[&[&str]] = &[
        &["koshi", "new"],
        &["koshi", "list-sessions"],
        &["koshi", "kill-session"],
        &["koshi", "doctor"],
        &["koshi", "config"],
        &["koshi", "plugin"],
        &["koshi", "actions"],
        &[
            "koshi",
            "inspect",
            "pane",
            "pane-0192f0c1-2345-7000-8000-000000000001",
        ],
        &["koshi", "list-tabs"],
        &["koshi", "list-panes"],
        &["koshi", "list-clients"],
        &["koshi", "keys"],
    ];
    for argv in argvs {
        assert_eq!(command(argv).to_action(), None, "for {argv:?}");
    }
}
