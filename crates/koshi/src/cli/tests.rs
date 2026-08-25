//! Parse tests for the `koshi` command-line grammar: the bare interactive
//! launch, the headless launch, lifecycle commands, the typed action
//! subcommands and their command mapping, and usage-error diagnostics.

use clap::error::ErrorKind;
use clap::CommandFactory;
use clap::Parser;
use koshi_config::app_config::parse_app_config;
use koshi_core::action::{core_action_seeds, ActionHandlerRef};
use std::path::Path;

use super::*;
use uuid::Uuid;

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

/// The `(action, command)` pair the subcommand of `argv` maps to, for a CLI
/// with no `koshi.kdl` — its `layout.new-pane-direction` is the built-in
/// `Right`.
fn action_of(argv: &[&str]) -> (ActionRef, Command) {
    action_of_for(argv, Direction::Right)
}

/// [`action_of`] for a CLI whose own `layout.new-pane-direction` is
/// `new_pane_direction`.
fn action_of_for(argv: &[&str], new_pane_direction: Direction) -> (ActionRef, Command) {
    command(argv)
        .to_action(&ResolvedTargets::default(), new_pane_direction)
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
            headless: false,
            allow_other_users: false,
            profile: None,
            remote: None,
            command: None,
        }
    );
    assert!(cli.is_interactive_launch());
}

#[test]
fn profile_names_a_launch_profile_and_stays_an_interactive_launch() {
    let cli = parse(&["koshi", "--profile", "dev"]);
    assert_eq!(cli.profile, Some("dev".to_string()));
    assert!(cli.is_interactive_launch());
}

#[test]
fn headless_creates_a_session_without_the_interactive_launch() {
    let cli = parse(&["koshi", "--headless"]);
    assert_eq!(
        cli,
        Cli {
            headless: true,
            allow_other_users: false,
            profile: None,
            remote: None,
            command: None,
        }
    );
    assert!(!cli.is_interactive_launch());
}

#[test]
fn headless_takes_the_other_users_flag_beside_it() {
    let cli = parse(&["koshi", "--headless", "--allow-other-users"]);
    assert_eq!(
        cli,
        Cli {
            headless: true,
            allow_other_users: true,
            profile: None,
            remote: None,
            command: None,
        }
    );
}

#[test]
fn the_other_users_flag_without_headless_is_a_usage_error() {
    // The flag only reaches a session this command creates, and only
    // `--headless` creates one here, so a bare `koshi --allow-other-users`
    // would silently do nothing.
    let error = parse_err(&["koshi", "--allow-other-users"]);

    assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn attach_without_a_session_picks_one_at_runtime() {
    let cli = parse(&["koshi", "attach"]);
    assert_eq!(
        cli,
        Cli {
            headless: false,
            allow_other_users: false,
            profile: None,
            remote: None,
            command: Some(CliCommand::Attach {
                session: None,
                save_as: None,
            }),
        }
    );
}

#[test]
fn attach_takes_the_session_as_a_positional() {
    let session = format!("session-{}", fixed_uuid());
    assert_eq!(
        parse(&["koshi", "attach", &session]).command,
        Some(CliCommand::Attach {
            session: Some(session),
            save_as: None,
        })
    );
}

#[test]
fn attach_takes_a_server_and_the_name_to_save_it_under() {
    let cli = parse(&[
        "koshi",
        "attach",
        "--remote",
        "laptop.local:7654",
        "--save-as",
        "work",
        "web",
    ]);
    assert_eq!(cli.remote, Some("laptop.local:7654".to_string()));
    assert_eq!(
        cli.command,
        Some(CliCommand::Attach {
            session: Some("web".to_string()),
            save_as: Some("work".to_string()),
        })
    );
}

#[test]
fn a_name_to_save_a_server_under_without_a_server_is_a_usage_error() {
    // The name only ever labels a server `--remote` reached, so there is
    // nothing for it to label on its own.
    let error = parse_err(&["koshi", "attach", "--save-as", "work"]);

    assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn the_server_flag_reaches_the_action_verbs_and_the_bare_invocation() {
    assert_eq!(
        parse(&["koshi", "new-pane", "--remote", "work"]).remote,
        Some("work".to_string())
    );
    assert_eq!(
        parse(&["koshi", "close-pane", "--remote", "work"]).remote,
        Some("work".to_string())
    );
    // A bare invocation parses here and is refused at dispatch, which has
    // nothing to run on the named machine.
    let bare = parse(&["koshi", "--remote", "work"]);
    assert_eq!(bare.remote, Some("work".to_string()));
    assert_eq!(bare.command, None);
}

#[test]
fn bare_detach_names_no_target_and_no_session() {
    let cli = parse(&["koshi", "detach"]);
    assert_eq!(
        cli,
        Cli {
            headless: false,
            allow_other_users: false,
            profile: None,
            remote: None,
            command: Some(CliCommand::Detach {
                target: None,
                all: false,
            }),
        }
    );
}

#[test]
fn detach_takes_the_client_as_a_positional() {
    assert_eq!(
        parse(&["koshi", "detach", "3f2a"]).command,
        Some(CliCommand::Detach {
            target: Some("3f2a".to_string()),
            all: false,
        })
    );
}

#[test]
fn detach_all_without_a_session_names_no_target() {
    assert_eq!(
        parse(&["koshi", "detach", "--all"]).command,
        Some(CliCommand::Detach {
            target: None,
            all: true,
        })
    );
}

#[test]
fn detach_all_takes_the_session_as_a_positional() {
    let session = format!("session-{}", fixed_uuid());
    assert_eq!(
        parse(&["koshi", "detach", "--all", &session]).command,
        Some(CliCommand::Detach {
            target: Some(session),
            all: true,
        })
    );
}

#[test]
fn the_removed_attach_and_detach_root_flags_are_usage_errors() {
    for argv in [
        ["koshi", "--attach", "x"].as_slice(),
        ["koshi", "--detach"].as_slice(),
        ["koshi", "--detach-all"].as_slice(),
    ] {
        let err = parse_err(argv);
        assert_eq!(err.kind(), ErrorKind::UnknownArgument);
        assert_eq!(err.exit_code(), 2);
    }
}

#[test]
fn headless_conflicts_with_subcommands() {
    let err = parse_err(&["koshi", "--headless", "list-sessions"]);
    assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
}

#[test]
fn the_removed_new_verb_is_a_usage_error() {
    let err = parse_err(&["koshi", "new"]);
    assert_eq!(err.kind(), ErrorKind::InvalidSubcommand);
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn lifecycle_commands_parse() {
    assert_eq!(
        parse(&["koshi", "list-sessions"]).command,
        Some(CliCommand::ListSessions {
            format: FormatArg::Table
        })
    );
    assert_eq!(
        parse(&["koshi", "doctor"]).command,
        Some(CliCommand::Doctor {
            format: FormatArg::Table
        })
    );
}

#[test]
fn doctor_takes_a_format() {
    assert_eq!(
        parse(&["koshi", "doctor", "--format", "json"]).command,
        Some(CliCommand::Doctor {
            format: FormatArg::Json
        })
    );
}

#[test]
fn a_subcommand_is_not_the_interactive_launch() {
    assert!(!parse(&["koshi", "list-sessions"]).is_interactive_launch());
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
            session: Some(SessionRef::Name("work".to_string()))
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
        (
            "new-tab",
            CliCommand::NewTab {
                session: None,
                client: None,
            },
        ),
        ("next-tab", CliCommand::NextTab { client: None }),
        ("previous-tab", CliCommand::PreviousTab { client: None }),
        ("lock", CliCommand::Lock { client: None }),
        ("unlock", CliCommand::Unlock { client: None }),
        ("toggle-lock", CliCommand::ToggleLock { client: None }),
        ("plugin", CliCommand::Plugin),
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
    ];
    for (name, expected) in cases {
        assert_eq!(parse(&["koshi", name]).command.as_ref(), Some(expected));
    }
}

#[test]
fn config_subcommands_parse_without_a_default_command() {
    assert_eq!(
        parse(&["koshi", "config", "path"]).command,
        Some(CliCommand::Config {
            command: ConfigCommand::Path,
        })
    );
    assert_eq!(
        parse(&["koshi", "config", "explain", "koshi.pane.min-cols"]).command,
        Some(CliCommand::Config {
            command: ConfigCommand::Explain {
                key: "koshi.pane.min-cols".to_string(),
            },
        })
    );
    assert_eq!(
        parse(&["koshi", "config", "check"]).command,
        Some(CliCommand::Config {
            command: ConfigCommand::Check,
        })
    );
    assert_eq!(
        parse(&["koshi", "config", "migrate"]).command,
        Some(CliCommand::Config {
            command: ConfigCommand::Migrate,
        })
    );
    assert_eq!(
        parse_err(&["koshi", "config"]).kind(),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    assert_eq!(
        parse_err(&["koshi", "config", "default"]).kind(),
        ErrorKind::InvalidSubcommand
    );
}

// --- Debug subcommands ---

#[test]
fn dump_state_defaults_to_the_table_format() {
    assert_eq!(
        parse(&["koshi", "debug", "dump-state"]).command,
        Some(CliCommand::Debug {
            command: DebugCommand::DumpState {
                format: FormatArg::Table,
            },
        })
    );
}

#[test]
fn dump_state_takes_the_json_format() {
    assert_eq!(
        parse(&["koshi", "debug", "dump-state", "--format", "json"]).command,
        Some(CliCommand::Debug {
            command: DebugCommand::DumpState {
                format: FormatArg::Json,
            },
        })
    );
}

#[test]
fn dump_layout_defaults_to_every_tab_and_the_table_format() {
    assert_eq!(
        parse(&["koshi", "debug", "dump-layout"]).command,
        Some(CliCommand::Debug {
            command: DebugCommand::DumpLayout {
                tab: None,
                format: FormatArg::Table,
            },
        })
    );
}

#[test]
fn dump_layout_takes_a_tab_id() {
    let tab = format!("tab-{}", fixed_uuid());
    assert_eq!(
        parse(&["koshi", "debug", "dump-layout", "--tab", &tab]).command,
        Some(CliCommand::Debug {
            command: DebugCommand::DumpLayout {
                tab: Some(TabRef::Id(TabId::from_uuid(fixed_uuid()))),
                format: FormatArg::Table,
            },
        })
    );
}

#[test]
fn dump_layout_takes_a_tab_name() {
    assert_eq!(
        parse(&["koshi", "debug", "dump-layout", "--tab", "editor"]).command,
        Some(CliCommand::Debug {
            command: DebugCommand::DumpLayout {
                tab: Some(TabRef::Name("editor".to_string())),
                format: FormatArg::Table,
            },
        })
    );
}

#[test]
fn dump_layout_takes_a_tab_and_the_json_format_together() {
    assert_eq!(
        parse(&[
            "koshi",
            "debug",
            "dump-layout",
            "--tab",
            "editor",
            "--format",
            "json",
        ])
        .command,
        Some(CliCommand::Debug {
            command: DebugCommand::DumpLayout {
                tab: Some(TabRef::Name("editor".to_string())),
                format: FormatArg::Json,
            },
        })
    );
}

#[test]
fn bare_debug_requires_a_subcommand() {
    assert_eq!(
        parse_err(&["koshi", "debug"]).kind(),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
}

#[test]
fn an_unknown_debug_subcommand_is_refused() {
    assert_eq!(
        parse_err(&["koshi", "debug", "dump-everything"]).kind(),
        ErrorKind::InvalidSubcommand
    );
}

#[test]
fn an_unknown_dump_format_is_refused() {
    assert_eq!(
        parse_err(&["koshi", "debug", "dump-state", "--format", "yaml"]).kind(),
        ErrorKind::InvalidValue
    );
}

#[test]
fn the_debug_dumps_are_queries_and_map_to_no_action() {
    for argv in [
        vec!["koshi", "debug", "dump-state"],
        vec!["koshi", "debug", "dump-layout"],
    ] {
        assert_eq!(
            command(&argv).to_action(&ResolvedTargets::default(), Direction::Right),
            None,
            "{argv:?} must stay a read-only query",
        );
    }
}

// --- Keys subcommands ---

#[test]
fn bare_keys_requires_a_subcommand() {
    let err = parse_err(&["koshi", "keys"]);
    assert_eq!(
        err.kind(),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
}

#[test]
fn keys_list_parses_its_filters_and_format() {
    assert_eq!(
        command(&["koshi", "keys", "list"]),
        CliCommand::Keys {
            command: KeysCommand::List {
                mode: None,
                scope: None,
                recommended: false,
                format: FormatArg::Table,
            }
        }
    );
    assert_eq!(
        command(&[
            "koshi",
            "keys",
            "list",
            "--mode",
            "locked",
            "--scope",
            "user",
            "--recommended",
            "--format",
            "json",
        ]),
        CliCommand::Keys {
            command: KeysCommand::List {
                mode: Some("locked".to_string()),
                scope: Some(ScopeArg::User),
                recommended: true,
                format: FormatArg::Json,
            }
        }
    );
}

#[test]
fn keys_describe_parses_the_sequence() {
    assert_eq!(
        command(&["koshi", "keys", "describe", "<C-p> n"]),
        CliCommand::Keys {
            command: KeysCommand::Describe {
                sequence: "<C-p> n".to_string(),
                format: FormatArg::Table,
            }
        }
    );
}

#[test]
fn keys_conflicts_parses_a_format() {
    assert_eq!(
        command(&["koshi", "keys", "conflicts", "--format", "json"]),
        CliCommand::Keys {
            command: KeysCommand::Conflicts {
                format: FormatArg::Json,
            }
        }
    );
}

#[test]
fn keys_validate_parses_the_path() {
    assert_eq!(
        command(&["koshi", "keys", "validate", "my-keys.kdl"]),
        CliCommand::Keys {
            command: KeysCommand::Validate {
                path: PathBuf::from("my-keys.kdl"),
                format: FormatArg::Table,
            }
        }
    );
}

#[test]
fn keys_mutation_verbs_do_not_exist() {
    // Keybindings mutate through `keybinding.kdl` only; the `keys` tree is
    // read-only introspection.
    for verb in ["set", "remove", "reset"] {
        let err = parse_err(&["koshi", "keys", verb]);
        assert_eq!(err.kind(), ErrorKind::InvalidSubcommand, "for {verb}");
    }
}

#[test]
fn keys_queries_map_to_no_action() {
    for argv in [
        vec!["koshi", "keys", "list"],
        vec!["koshi", "keys", "describe", "<C-y>"],
        vec!["koshi", "keys", "conflicts"],
        vec!["koshi", "keys", "validate", "f.kdl"],
    ] {
        assert_eq!(
            command(&argv).to_action(&ResolvedTargets::default(), Direction::Right),
            None
        );
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
            session: Some(SessionRef::Id(SessionId::from_uuid(fixed_uuid()))),
            format: FormatArg::Json,
        })
    );
}

#[test]
fn list_panes_parses_a_session_filter() {
    let session = format!("session-{}", fixed_uuid());
    assert_eq!(
        parse(&["koshi", "list-panes", "--session", &session]).command,
        Some(CliCommand::ListPanes {
            session: Some(SessionRef::Id(SessionId::from_uuid(fixed_uuid()))),
            format: FormatArg::Table,
        })
    );
}

#[test]
fn list_panes_takes_no_tab_filter() {
    let tab = format!("tab-{}", fixed_uuid());
    assert_eq!(
        parse_err(&["koshi", "list-panes", "--tab", &tab]).kind(),
        ErrorKind::UnknownArgument
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
                session: SessionRef::Id(SessionId::from_uuid(uuid)),
                format: FormatArg::Table,
            },
        ),
        (
            "tab",
            format!("tab-{uuid}"),
            InspectTarget::Tab {
                tab: TabRef::Id(TabId::from_uuid(uuid)),
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

// --- Action introspection ---

#[test]
fn actions_list_parses_with_a_default_and_a_json_format() {
    assert_eq!(
        parse(&["koshi", "actions", "list"]).command,
        Some(CliCommand::Actions {
            command: ActionsCommand::List {
                format: FormatArg::Table,
            },
        })
    );
    assert_eq!(
        parse(&["koshi", "actions", "list", "--format", "json"]).command,
        Some(CliCommand::Actions {
            command: ActionsCommand::List {
                format: FormatArg::Json,
            },
        })
    );
}

#[test]
fn actions_explain_takes_an_action_name_and_a_format() {
    assert_eq!(
        parse(&["koshi", "actions", "explain", "new-pane"]).command,
        Some(CliCommand::Actions {
            command: ActionsCommand::Explain {
                action: "new-pane".to_string(),
                format: FormatArg::Table,
            },
        })
    );
    assert_eq!(
        parse(&[
            "koshi",
            "actions",
            "explain",
            "core:new-pane",
            "--format",
            "json"
        ])
        .command,
        Some(CliCommand::Actions {
            command: ActionsCommand::Explain {
                action: "core:new-pane".to_string(),
                format: FormatArg::Json,
            },
        })
    );
}

#[test]
fn actions_requires_a_subcommand() {
    let err = parse_err(&["koshi", "actions"]);
    assert_eq!(
        err.kind(),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn actions_explain_requires_an_action() {
    let err = parse_err(&["koshi", "actions", "explain"]);
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
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
        "attach",
        "close-pane",
        "close-tab",
        "config",
        "debug",
        "detach",
        "doctor",
        "focus-pane",
        "focus-tab",
        "input",
        "inspect",
        "keys",
        "kill-session",
        "list-clients",
        "list-panes",
        "list-sessions",
        "list-tabs",
        "lock",
        "move-tab",
        "new-pane",
        "new-tab",
        "next-tab",
        "plugin",
        "previous-tab",
        "remote",
        "resize-pane",
        "resume-support",
        "run",
        "serve-pty-supervisor",
        "serve-router",
        "serve-session",
        "server-version",
        "share",
        "toggle-lock",
        "toggle-pane-fullscreen",
        "unlock",
        "update",
        "version",
    ]
    .map(String::from)
    .to_vec();
    expected.sort();
    assert_eq!(names, expected);
}

#[test]
fn serve_router_takes_the_wait_for_lock_flag() {
    // The flag is what a router handing its place over passes to the router
    // it starts, so the new one waits instead of yielding to the old one.
    assert_eq!(
        parse(&["koshi", "serve-router", "--runtime-dir", "X"]).command,
        Some(CliCommand::ServeRouter {
            runtime_dir: Some(PathBuf::from("X")),
            wait_for_lock: false,
        })
    );
    assert_eq!(
        parse(&[
            "koshi",
            "serve-router",
            "--runtime-dir",
            "X",
            "--wait-for-lock"
        ])
        .command,
        Some(CliCommand::ServeRouter {
            runtime_dir: Some(PathBuf::from("X")),
            wait_for_lock: true,
        })
    );
    assert_eq!(
        parse(&["koshi", "serve-router"]).command,
        Some(CliCommand::ServeRouter {
            runtime_dir: None,
            wait_for_lock: false,
        })
    );
}

#[test]
fn the_help_hides_the_self_run_subcommands_and_the_unwired_plugin_verb() {
    let hidden: Vec<String> = Cli::command()
        .get_subcommands()
        .filter(|command| command.is_hide_set())
        .map(|command| command.get_name().to_string())
        .collect();

    assert_eq!(
        hidden,
        [
            "plugin",
            "serve-router",
            "serve-session",
            "serve-pty-supervisor",
            "resume-support"
        ]
    );
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

#[test]
fn an_unknown_flag_prints_the_flag_and_the_usage_lines() {
    assert_eq!(
        parse_err(&["koshi", "--frobnicate"]).to_string(),
        "error: unexpected argument '--frobnicate' found\n\n\
         Usage: koshi [OPTIONS]\n       koshi <COMMAND>\n\n\
         For more information, try '--help'.\n"
    );
}

#[test]
fn an_unknown_subcommand_prints_the_subcommand_and_the_usage_lines() {
    assert_eq!(
        parse_err(&["koshi", "explode"]).to_string(),
        "error: unrecognized subcommand 'explode'\n\n\
         Usage: koshi [OPTIONS]\n       koshi <COMMAND>\n\n\
         For more information, try '--help'.\n"
    );
}

#[test]
fn an_invalid_direction_prints_every_direction_it_accepts() {
    assert_eq!(
        parse_err(&["koshi", "new-pane", "--direction", "sideways"]).to_string(),
        "error: invalid value 'sideways' for '--direction <DIRECTION>'\n  \
         [possible values: right, down, left, up]\n\n\
         For more information, try '--help'.\n"
    );
}

/// `lock` acts on a client, so `--session` is not one of its flags: the
/// argument grammar refuses it before any session is reached.
#[test]
fn a_session_target_on_lock_prints_the_flag_and_the_lock_usage_line() {
    assert_eq!(
        parse_err(&["koshi", "lock", "--session", "work"]).to_string(),
        "error: unexpected argument '--session' found\n\n\
         Usage: koshi lock [OPTIONS]\n\n\
         For more information, try '--help'.\n"
    );
}

/// The rendered help of one verb, byte for byte: the about line, the usage
/// line, and every flag with its own help and default.
#[test]
fn resize_pane_help_renders_its_about_usage_and_flags() {
    assert_eq!(
        parse_err(&["koshi", "resize-pane", "--help"]).to_string(),
        "Move one of a pane's borders: a positive size grows the pane toward the direction, \
         a negative size shrinks it\n\n\
         Usage: koshi resize-pane [OPTIONS] --direction <DIRECTION>\n\n\
         Options:\n      \
         --direction <DIRECTION>\n          Which of the pane's borders moves\n\n          \
         Possible values:\n          \
         - right: Rightward\n          \
         - down:  Downward\n          \
         - left:  Leftward\n          \
         - up:    Upward\n\n      \
         --size <SIZE>\n          \
         Signed number of cells the border moves; defaults to 1\n          \n          \
         [default: 1]\n\n      \
         --pane <PANE_ID>\n          Pane to resize; defaults to the focused pane\n\n      \
         --remote <SERVER>\n          \
         Run this invocation against the machine SERVER names — the name it was saved under, \
         or the `host:port` it listens on — instead of this one\n\n  \
         -h, --help\n          Print help (see a summary with '-h')\n"
    );
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
            session: None,
            tab: None,
            client: None,
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
            session: None,
            tab: None,
            client: None,
        }
    );
    assert_eq!(
        command(&["koshi", "new-pane", "--stacked"]),
        CliCommand::NewPane {
            direction: None,
            stacked: true,
            pane: None,
            session: None,
            tab: None,
            client: None,
        }
    );
}

#[test]
fn new_pane_direction_and_stacked_conflict() {
    let err = parse_err(&["koshi", "new-pane", "--direction", "left", "--stacked"]);
    assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    assert_eq!(err.exit_code(), 2);
}

/// The CLI is a client: with no `--direction` it opens the pane the way this
/// machine's `koshi.kdl` says, through the real `koshi.kdl` parser and the
/// real client fold. A CLI that ignored the file would split `Right` here.
#[test]
fn new_pane_without_a_direction_flag_follows_the_config_file() {
    let file = parse_app_config(
        Path::new("koshi.kdl"),
        "version 1\nlayout {\n    new-pane-direction \"down\"\n}\n",
    )
    .expect("the fixture parses");
    let configured = koshi_link::config::new_pane_direction(Some(file.layer));
    assert_eq!(configured, Direction::Down, "the file's own value");

    let (_, mapped) = action_of_for(&["koshi", "new-pane"], configured);
    assert_eq!(
        mapped,
        Command::NewPane(NewPaneArgs {
            source: None,
            tab: None,
            direction: Direction::Down,
            stacked: false,
            cwd: None,
            command: None,
            client: None,
        })
    );
}

/// An explicit `--direction` beats the file, and `run` reads the file the same
/// way `new-pane` does.
#[test]
fn an_explicit_direction_flag_wins_over_the_config_file() {
    let (_, mapped) = action_of_for(
        &["koshi", "new-pane", "--direction", "left"],
        Direction::Down,
    );
    let Command::NewPane(args) = mapped else {
        panic!("new-pane maps to NewPane");
    };
    assert_eq!(args.direction, Direction::Left);

    let (_, mapped) = action_of_for(&["koshi", "run", "--", "htop"], Direction::Down);
    let Command::RunCommandPane(args) = mapped else {
        panic!("run maps to RunCommandPane");
    };
    assert_eq!(args.direction, Direction::Down);
}

/// No config directory, no `koshi.kdl`, or a file that did not parse: the fold
/// leaves the built-in `Right`.
#[test]
fn no_config_file_leaves_the_built_in_split_direction() {
    assert_eq!(
        koshi_link::config::new_pane_direction(None),
        Direction::Right
    );

    let (_, mapped) = action_of(&["koshi", "new-pane"]);
    let Command::NewPane(args) = mapped else {
        panic!("new-pane maps to NewPane");
    };
    assert_eq!(args.direction, Direction::Right);
}

#[test]
fn new_pane_parses_session_tab_and_client_targets() {
    let client_flag = format!("client-{}", fixed_uuid());
    assert_eq!(
        command(&[
            "koshi",
            "new-pane",
            "--session",
            "amber-fox",
            "--tab",
            "logs",
            "--client",
            &client_flag
        ]),
        CliCommand::NewPane {
            direction: None,
            stacked: false,
            pane: None,
            session: Some(SessionRef::Name("amber-fox".to_string())),
            tab: Some(TabRef::Name("logs".to_string())),
            client: Some(ClientId::from_uuid(fixed_uuid())),
        }
    );
}

#[test]
fn new_pane_tab_given_as_an_id_reaches_the_command_without_a_lookup() {
    // A `--tab` id needs no session lookup: `to_action` with no resolved
    // targets still carries it into the command's `tab` field.
    let tab_flag = format!("tab-{}", fixed_uuid());
    let (_, mapped) = action_of(&["koshi", "new-pane", "--tab", &tab_flag]);
    assert_eq!(
        mapped,
        Command::NewPane(NewPaneArgs {
            source: None,
            tab: Some(TabId::from_uuid(fixed_uuid())),
            direction: Direction::Right,
            stacked: false,
            cwd: None,
            command: None,
            client: None,
        })
    );
}

#[test]
fn new_pane_pane_and_tab_conflict() {
    let pane_flag = format!("pane-{}", fixed_uuid());
    let err = parse_err(&["koshi", "new-pane", "--pane", &pane_flag, "--tab", "logs"]);
    assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn lock_verbs_take_an_optional_client() {
    let client_flag = format!("client-{}", fixed_uuid());
    let client = Some(ClientId::from_uuid(fixed_uuid()));
    assert_eq!(
        command(&["koshi", "lock", "--client", &client_flag]),
        CliCommand::Lock { client }
    );
    let (_, mapped) = action_of(&["koshi", "lock", "--client", &client_flag]);
    assert_eq!(
        mapped,
        Command::SetLockMode(LockModeArgs {
            locked: true,
            client,
        })
    );
    let (_, mapped) = action_of(&["koshi", "unlock", "--client", &client_flag]);
    assert_eq!(
        mapped,
        Command::SetLockMode(LockModeArgs {
            locked: false,
            client,
        })
    );
    let (_, mapped) = action_of(&["koshi", "toggle-lock", "--client", &client_flag]);
    assert_eq!(
        mapped,
        Command::ToggleLockMode(ToggleLockModeArgs { client })
    );
}

#[test]
fn new_tab_takes_an_optional_session_and_client() {
    let client_flag = format!("client-{}", fixed_uuid());
    let client = ClientId::from_uuid(fixed_uuid());
    assert_eq!(
        command(&["koshi", "new-tab", "--session", "amber-fox"]),
        CliCommand::NewTab {
            session: Some(SessionRef::Name("amber-fox".to_string())),
            client: None,
        }
    );
    assert_eq!(
        command(&[
            "koshi",
            "new-tab",
            "--session",
            "amber-fox",
            "--client",
            &client_flag
        ]),
        CliCommand::NewTab {
            session: Some(SessionRef::Name("amber-fox".to_string())),
            client: Some(client),
        }
    );
    assert_eq!(
        command(&["koshi", "new-tab", "--session", "amber-fox"]).target_session(),
        Some(&SessionRef::Name("amber-fox".to_string()))
    );
}

#[test]
fn new_tab_carries_its_client_into_the_command_and_the_routing_target() {
    let client_flag = format!("client-{}", fixed_uuid());
    let client = ClientId::from_uuid(fixed_uuid());
    let parsed = command(&["koshi", "new-tab", "--client", &client_flag]);
    assert_eq!(parsed.target_client(), Some(client));
    assert_eq!(parsed.target_session(), None);
    let (_, mapped) = action_of(&["koshi", "new-tab", "--client", &client_flag]);
    assert_eq!(
        mapped,
        Command::NewTab(NewTabArgs {
            cwd: None,
            client: Some(client),
        })
    );

    // With no flag the command names no client.
    let bare = command(&["koshi", "new-tab"]);
    assert_eq!(bare.target_client(), None);
}

#[test]
fn new_tab_client_value_must_read_as_a_client_id() {
    let err = parse_err(&["koshi", "new-tab", "--client", "amber-fox"]);
    assert_eq!(err.kind(), ErrorKind::ValueValidation);
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
                tab: None,
                direction: *expected,
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
fn input_parses_its_text_target_and_enter_flag() {
    assert_eq!(
        command(&["koshi", "input", "ls"]),
        CliCommand::Input {
            text: "ls".to_string(),
            pane: None,
            no_enter: false,
        }
    );
    let pane_flag = format!("pane-{}", fixed_uuid());
    assert_eq!(
        command(&[
            "koshi",
            "input",
            "--pane",
            &pane_flag,
            "--no-enter",
            "ls -la"
        ]),
        CliCommand::Input {
            text: "ls -la".to_string(),
            pane: Some(PaneId::from_uuid(fixed_uuid())),
            no_enter: true,
        }
    );
}

/// Text that starts with `-` is text, not a flag: a script piping arbitrary
/// lines into a pane cannot control whether one begins with a dash, and
/// `koshi input "-la"` must type `-la` rather than fail as an unknown flag.
/// The real flags keep working on both sides of it.
#[test]
fn input_takes_text_that_starts_with_a_dash() {
    assert_eq!(
        command(&["koshi", "input", "-la"]),
        CliCommand::Input {
            text: "-la".to_string(),
            pane: None,
            no_enter: false,
        }
    );

    // A flag AFTER the text is still a flag, not more text.
    assert_eq!(
        command(&["koshi", "input", "ls", "--no-enter"]),
        CliCommand::Input {
            text: "ls".to_string(),
            pane: None,
            no_enter: true,
        }
    );
}

#[test]
fn input_requires_its_text() {
    let err = parse_err(&["koshi", "input"]);
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}

/// The text is sent as typed, and Enter — the carriage return a shell reads as
/// "run this line" — is appended unless `--no-enter` holds it back.
#[test]
fn input_appends_enter_unless_no_enter_is_given() {
    let pane_flag = format!("pane-{}", fixed_uuid());

    let (action, command) = action_of(&["koshi", "input", "--pane", &pane_flag, "ls"]);
    assert_eq!(action, ActionRef::core("write-to-pane").expect("valid"));
    assert_eq!(
        command,
        Command::WriteToPane(WriteToPaneArgs {
            pane: Some(PaneId::from_uuid(fixed_uuid())),
            data: b"ls\r".to_vec(),
        })
    );

    let (_, command) = action_of(&["koshi", "input", "--no-enter", "ls"]);
    assert_eq!(
        command,
        Command::WriteToPane(WriteToPaneArgs {
            pane: None,
            data: b"ls".to_vec(),
        })
    );
}

#[test]
fn close_tab_parses_target_and_force() {
    let tab_flag = format!("tab-{}", fixed_uuid());
    assert_eq!(
        command(&["koshi", "close-tab", "--tab", &tab_flag, "--force"]),
        CliCommand::CloseTab {
            tab: Some(TabRef::Id(TabId::from_uuid(fixed_uuid()))),
            session: None,
            force: true,
        }
    );
    // A value that does not read as a tab id is taken as a tab name.
    assert_eq!(
        command(&["koshi", "close-tab", "--tab", "logs"]),
        CliCommand::CloseTab {
            tab: Some(TabRef::Name("logs".to_string())),
            session: None,
            force: false,
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
            tab: Some(TabRef::Id(TabId::from_uuid(fixed_uuid()))),
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
fn run_takes_its_command_after_the_separator() {
    assert_eq!(
        command(&["koshi", "run", "--", "htop", "-d", "5"]),
        CliCommand::Run {
            direction: None,
            stacked: false,
            pane: None,
            session: None,
            tab: None,
            client: None,
            command: vec!["htop".to_string(), "-d".to_string(), "5".to_string()],
        }
    );
    assert_eq!(
        command(&["koshi", "run", "--direction", "down", "--", "htop"]),
        CliCommand::Run {
            direction: Some(DirectionArg::Down),
            stacked: false,
            pane: None,
            session: None,
            tab: None,
            client: None,
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
            session: None,
            tab: None,
            client: None,
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
            tab: None,
            direction: Direction::Right,
            stacked: false,
            client: None,
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
    let cases: Vec<(Vec<&str>, &str, Command)> = vec![
        (
            vec!["koshi", "new-pane", "--direction", "right"],
            "new-pane",
            Command::NewPane(NewPaneArgs {
                source: None,
                tab: None,
                direction: Direction::Right,
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
                tab: None,
                direction: Direction::Right,
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
                tree: false,
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
                tree: false,
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
            Command::FocusPane(FocusPaneArgs {
                target: FocusTarget::Pane(pane),
                client: None,
            }),
        ),
        (
            vec!["koshi", "lock"],
            "lock",
            Command::SetLockMode(LockModeArgs {
                locked: true,
                client: None,
            }),
        ),
        (
            vec!["koshi", "unlock"],
            "unlock",
            Command::SetLockMode(LockModeArgs {
                locked: false,
                client: None,
            }),
        ),
        (
            vec!["koshi", "toggle-lock"],
            "toggle-lock",
            Command::ToggleLockMode(ToggleLockModeArgs::default()),
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
                tab: None,
                direction: Direction::Right,
                stacked: true,
                client: None,
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
        &["koshi", "new-tab"],
        &["koshi", "close-tab"],
        &["koshi", "next-tab"],
        &["koshi", "previous-tab"],
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
        &["koshi", "list-sessions"],
        &["koshi", "kill-session"],
        &["koshi", "attach"],
        &["koshi", "detach"],
        &["koshi", "doctor"],
        &["koshi", "config", "path"],
        &["koshi", "plugin"],
        &["koshi", "actions", "list"],
        &[
            "koshi",
            "inspect",
            "pane",
            "pane-0192f0c1-2345-7000-8000-000000000001",
        ],
        &["koshi", "list-tabs"],
        &["koshi", "list-panes"],
        &["koshi", "list-clients"],
        &["koshi", "keys", "list"],
    ];
    for argv in argvs {
        assert_eq!(
            command(argv).to_action(&ResolvedTargets::default(), Direction::Right),
            None,
            "for {argv:?}"
        );
    }
}

// --- Adversarial: duplicate flags, boundaries, and unicode ---

#[test]
fn a_repeated_single_valued_flag_is_a_usage_error_not_a_last_wins() {
    // clap's derived args do not override themselves by default: giving the
    // same single-valued flag twice is a hard usage error, not "the last one
    // wins" — true for a root flag (`--profile`) and a subcommand flag
    // (`--format`) alike.
    let profile_twice = parse_err(&["koshi", "--profile", "first", "--profile", "second"]);
    assert_eq!(profile_twice.kind(), ErrorKind::ArgumentConflict);
    assert_eq!(profile_twice.exit_code(), 2);

    let format_twice = parse_err(&[
        "koshi",
        "list-sessions",
        "--format",
        "json",
        "--format",
        "table",
    ]);
    assert_eq!(format_twice.kind(), ErrorKind::ArgumentConflict);
}

#[test]
fn attach_accepts_an_empty_session_id() {
    // `attach` stores the raw string untyped; validation is a runtime concern,
    // not a parse concern, so an empty value still parses.
    assert_eq!(
        parse(&["koshi", "attach", ""]).command,
        Some(CliCommand::Attach {
            session: Some(String::new()),
            save_as: None,
        })
    );
}

#[test]
fn attach_accepts_a_unicode_session_id() {
    assert_eq!(
        parse(&["koshi", "attach", "café-上海"]).command,
        Some(CliCommand::Attach {
            session: Some("café-上海".to_string()),
            save_as: None,
        })
    );
}

#[test]
fn focus_tab_index_rejects_a_negative_number() {
    let err = parse_err(&["koshi", "focus-tab", "--index", "-1"]);
    assert_eq!(err.kind(), ErrorKind::UnknownArgument);
}

#[test]
fn focus_tab_index_rejects_an_overflowing_number() {
    // One digit past `usize::MAX` (18446744073709551615 on a 64-bit target).
    let err = parse_err(&["koshi", "focus-tab", "--index", "18446744073709551616"]);
    assert_eq!(err.kind(), ErrorKind::ValueValidation);
}

#[test]
fn resize_pane_size_accepts_the_i16_boundaries() {
    assert_eq!(
        command(&[
            "koshi",
            "resize-pane",
            "--direction",
            "up",
            "--size",
            "32767"
        ]),
        CliCommand::ResizePane {
            direction: DirectionArg::Up,
            size: i16::MAX,
            pane: None,
        }
    );
    assert_eq!(
        command(&[
            "koshi",
            "resize-pane",
            "--direction",
            "up",
            "--size",
            "-32768"
        ]),
        CliCommand::ResizePane {
            direction: DirectionArg::Up,
            size: i16::MIN,
            pane: None,
        }
    );
}

#[test]
fn resize_pane_size_rejects_i16_overflow() {
    let err = parse_err(&[
        "koshi",
        "resize-pane",
        "--direction",
        "up",
        "--size",
        "32768",
    ]);
    assert_eq!(err.kind(), ErrorKind::ValueValidation);
}

#[test]
fn format_value_is_case_sensitive() {
    let err = parse_err(&["koshi", "list-sessions", "--format", "Table"]);
    assert_eq!(err.kind(), ErrorKind::InvalidValue);
}

#[test]
fn an_id_with_only_the_prefix_and_a_dash_is_rejected() {
    let err = parse_err(&["koshi", "close-pane", "--pane", "pane-"]);
    assert_eq!(err.kind(), ErrorKind::ValueValidation);
}

#[test]
fn a_prefix_collision_without_a_separating_dash_is_rejected() {
    // "panes-<uuid>" strips as far as "pane" (a true prefix of "panes"),
    // leaving "s-<uuid>" — which does not start with '-', so the dash-strip
    // fails and the whole original string is tried as a bare UUID, which it
    // is not.
    let value = format!("panes-{}", fixed_uuid());
    let err = parse_err(&["koshi", "close-pane", "--pane", &value]);
    assert_eq!(err.kind(), ErrorKind::ValueValidation);
}

#[test]
fn a_session_value_that_is_not_an_id_parses_as_a_name() {
    // `--session` takes a name or an id, so a value that does not read as an
    // id — here a mistyped "sessions-" prefix — is kept whole as a name; it
    // then fails at routing when no session bears it, not at parse time.
    let value = format!("sessions-{}", fixed_uuid());
    assert_eq!(
        command(&["koshi", "new-tab", "--session", &value]),
        CliCommand::NewTab {
            session: Some(SessionRef::Name(value)),
            client: None,
        }
    );
}

#[test]
fn id_parse_error_message_names_the_expected_forms() {
    let err = parse_err(&["koshi", "close-pane", "--pane", "not-a-uuid"]);
    assert!(
        err.to_string()
            .contains("expected `pane-<uuid>` or a bare UUID"),
        "unexpected message: {err}"
    );
}

#[test]
fn a_bare_uppercase_uuid_parses() {
    // The UUID's own hex digits are case-insensitive even though the
    // `<prefix>-` stripping is a case-sensitive byte match.
    let uppercase = fixed_uuid().to_string().to_uppercase();
    assert_eq!(
        command(&["koshi", "close-pane", "--pane", &uppercase]),
        CliCommand::ClosePane {
            pane: Some(PaneId::from_uuid(fixed_uuid())),
            force: false,
        }
    );
}

#[test]
fn run_accepts_an_empty_program_token() {
    assert_eq!(
        command(&["koshi", "run", "--", ""]),
        CliCommand::Run {
            direction: None,
            stacked: false,
            pane: None,
            session: None,
            tab: None,
            client: None,
            command: vec![String::new()],
        }
    );
    let (_, mapped) = action_of(&["koshi", "run", "--", ""]);
    assert_eq!(
        mapped,
        Command::RunCommandPane(RunCommandPaneArgs {
            command: SpawnSpec {
                program: PathBuf::new(),
                args: vec![],
                cwd: None,
                env: BTreeMap::new(),
                shell_kind: ShellKind::Other(String::new()),
            },
            cwd: None,
            source: None,
            tab: None,
            direction: Direction::Right,
            stacked: false,
            client: None,
        })
    );
}

#[test]
fn run_program_name_is_preserved_verbatim_for_non_ascii() {
    let (_, mapped) = action_of(&["koshi", "run", "--", "☕"]);
    let Command::RunCommandPane(args) = mapped else {
        panic!("expected RunCommandPane");
    };
    assert_eq!(args.command.program, PathBuf::from("☕"));
    assert_eq!(args.command.shell_kind, ShellKind::Other("☕".to_string()));
}

// --- Session and tab arguments: id or name, verb by verb ---

/// The [`SessionRef`] the session-taking verb in `argv` parsed.
fn parsed_session_ref(argv: &[&str]) -> SessionRef {
    match command(argv) {
        CliCommand::KillSession { session }
        | CliCommand::ListTabs { session, .. }
        | CliCommand::ListPanes { session, .. }
        | CliCommand::ListClients { session, .. } => session.expect("argv names a session"),
        CliCommand::Inspect {
            target: InspectTarget::Session { session, .. },
        } => session,
        other => panic!("argv names no session: {other:?}"),
    }
}

/// The [`TabRef`] the tab-taking verb in `argv` parsed.
fn parsed_tab_ref(argv: &[&str]) -> TabRef {
    match command(argv) {
        CliCommand::MoveTab { tab, .. } | CliCommand::FocusTab { tab, .. } => {
            tab.expect("argv names a tab")
        }
        CliCommand::Inspect {
            target: InspectTarget::Tab { tab, .. },
        } => tab,
        other => panic!("argv names no tab: {other:?}"),
    }
}

/// Every verb taking a session runs the one id-else-name gate:
/// `session-<uuid>` is an id, `work` is a name.
#[test]
fn every_session_argument_parses_an_id_or_a_name() {
    let id = format!("session-{}", fixed_uuid());
    let prefixes: &[&[&str]] = &[
        &["koshi", "kill-session"],
        &["koshi", "list-tabs", "--session"],
        &["koshi", "list-panes", "--session"],
        &["koshi", "list-clients", "--session"],
        &["koshi", "inspect", "session"],
    ];
    for prefix in prefixes {
        let mut by_id = prefix.to_vec();
        by_id.push(&id);
        assert_eq!(
            parsed_session_ref(&by_id),
            SessionRef::Id(SessionId::from_uuid(fixed_uuid())),
            "for {by_id:?}"
        );

        let mut by_name = prefix.to_vec();
        by_name.push("work");
        assert_eq!(
            parsed_session_ref(&by_name),
            SessionRef::Name("work".to_string()),
            "for {by_name:?}"
        );
    }
}

/// Every verb taking a tab runs the one id-else-name gate: `tab-<uuid>` is an
/// id, `logs` is a name.
#[test]
fn every_tab_argument_parses_an_id_or_a_name() {
    let id = format!("tab-{}", fixed_uuid());
    let prefixes: &[&[&str]] = &[
        &["koshi", "move-tab", "--index", "2", "--tab"],
        &["koshi", "focus-tab", "--tab"],
        &["koshi", "inspect", "tab"],
    ];
    for prefix in prefixes {
        let mut by_id = prefix.to_vec();
        by_id.push(&id);
        assert_eq!(
            parsed_tab_ref(&by_id),
            TabRef::Id(TabId::from_uuid(fixed_uuid())),
            "for {by_id:?}"
        );

        let mut by_name = prefix.to_vec();
        by_name.push("logs");
        assert_eq!(
            parsed_tab_ref(&by_name),
            TabRef::Name("logs".to_string()),
            "for {by_name:?}"
        );
    }
}

/// A `--tab` id rides into the mapped command with no resolved targets; a
/// `--tab` name rides in as the id the routing layer resolved it to.
#[test]
fn move_tab_and_focus_tab_carry_their_tab_id_into_the_command() {
    let tab = TabId::from_uuid(fixed_uuid());
    let tab_flag = format!("tab-{}", fixed_uuid());

    let (_, mapped) = action_of(&["koshi", "move-tab", "--index", "2", "--tab", &tab_flag]);
    assert_eq!(
        mapped,
        Command::MoveTab(MoveTabArgs {
            tab: Some(tab),
            index: 2,
        })
    );
    let (_, mapped) = action_of(&["koshi", "focus-tab", "--tab", &tab_flag]);
    assert_eq!(
        mapped,
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(tab),
            client: None,
        })
    );

    let resolved = ResolvedTargets {
        session: None,
        tab: Some(tab),
    };
    assert_eq!(
        command(&["koshi", "move-tab", "--index", "2", "--tab", "logs"])
            .to_action(&resolved, Direction::Right),
        Some((
            ActionRef::core("move-tab").expect("valid"),
            Command::MoveTab(MoveTabArgs {
                tab: Some(tab),
                index: 2,
            })
        ))
    );
    assert_eq!(
        command(&["koshi", "focus-tab", "--tab", "logs"]).to_action(&resolved, Direction::Right),
        Some((
            ActionRef::core("focus-tab").expect("valid"),
            Command::FocusTab(FocusTabArgs {
                target: TabTarget::Id(tab),
                client: None,
            })
        ))
    );
}

#[test]
fn the_version_verbs_parse() {
    assert_eq!(
        parse(&["koshi", "version"]).command,
        Some(CliCommand::Version {
            format: FormatArg::Table,
        })
    );
    assert_eq!(
        parse(&["koshi", "version", "--format", "json"]).command,
        Some(CliCommand::Version {
            format: FormatArg::Json,
        })
    );
    assert_eq!(
        parse(&["koshi", "server-version"]).command,
        Some(CliCommand::ServerVersion {
            session: None,
            format: FormatArg::Table,
        })
    );
    assert_eq!(
        parse(&["koshi", "server-version", "--format", "json"]).command,
        Some(CliCommand::ServerVersion {
            session: None,
            format: FormatArg::Json,
        })
    );
}

#[test]
fn server_version_takes_a_session_by_name_or_by_id() {
    let session = SessionId::new();
    assert_eq!(
        parse(&["koshi", "server-version", "--session", "work"]).command,
        Some(CliCommand::ServerVersion {
            session: Some(SessionRef::Name("work".to_string())),
            format: FormatArg::Table,
        })
    );
    assert_eq!(
        parse(&["koshi", "server-version", "--session", &session.to_string()]).command,
        Some(CliCommand::ServerVersion {
            session: Some(SessionRef::Id(session)),
            format: FormatArg::Table,
        })
    );
}

#[test]
fn server_version_rejects_an_unknown_format() {
    let err = parse_err(&["koshi", "server-version", "--format", "yaml"]);
    assert_eq!(err.kind(), ErrorKind::InvalidValue);
}

#[test]
fn neither_version_verb_is_an_action_the_socket_serves() {
    assert_eq!(
        command(&["koshi", "version"]).to_action(&ResolvedTargets::default(), Direction::Right),
        None
    );
    assert_eq!(
        command(&["koshi", "server-version"])
            .to_action(&ResolvedTargets::default(), Direction::Right),
        None
    );
}

/// `koshi --remote work` names another machine, so it is not the bare
/// invocation that paints a terminal here.
#[test]
fn a_bare_invocation_naming_a_server_is_not_the_interactive_launch() {
    let cli = parse(&["koshi", "--remote", "work"]);

    assert_eq!(
        cli,
        Cli {
            headless: false,
            allow_other_users: false,
            profile: None,
            remote: Some("work".to_string()),
            command: None,
        }
    );
    assert!(!cli.is_interactive_launch());
}

/// The discovery split: every `list-*` verb and every `inspect` form is a
/// discovery query, and an action verb is not.
#[test]
fn discovery_queries_are_the_listings_and_inspects() {
    let command = |argv: &[&str]| parse(argv).command.expect("a subcommand was given");

    assert!(command(&["koshi", "list-sessions"]).is_discovery());
    assert!(command(&["koshi", "list-tabs"]).is_discovery());
    assert!(command(&["koshi", "list-panes"]).is_discovery());
    assert!(command(&["koshi", "list-clients"]).is_discovery());
    assert!(command(&["koshi", "inspect", "session", "main"]).is_discovery());
    assert!(!command(&["koshi", "new-tab"]).is_discovery());
}

/// A discovery query names its one session — a listing's `--session` flag or
/// the session an `inspect session` targets — and spans all sessions
/// otherwise.
#[test]
fn a_discovery_query_names_its_session_scope() {
    let command = |argv: &[&str]| parse(argv).command.expect("a subcommand was given");

    assert_eq!(
        command(&["koshi", "list-tabs", "--session", "main"]).discovery_session(),
        Some(&SessionRef::Name("main".to_string()))
    );
    assert_eq!(
        command(&["koshi", "inspect", "session", "main"]).discovery_session(),
        Some(&SessionRef::Name("main".to_string()))
    );
    assert_eq!(command(&["koshi", "list-tabs"]).discovery_session(), None);
    assert_eq!(
        command(&["koshi", "list-sessions"]).discovery_session(),
        None
    );
}

/// Every action name `to_action` builds is a registered core action: a CLI
/// verb naming an action the registry does not hold would resolve to nothing
/// at dispatch, invisibly.
#[test]
fn every_to_action_name_is_a_registered_core_action() {
    use std::collections::BTreeSet;

    use koshi_core::action::core_action_seeds;

    let registered: BTreeSet<String> = core_action_seeds()
        .iter()
        .map(|(action, _)| action.to_string())
        .collect();

    // One argv per CLI verb that maps to an action.
    let pane = PaneId::new().to_string();
    let action_verbs: Vec<Vec<&str>> = vec![
        vec!["koshi", "new-pane"],
        vec!["koshi", "close-pane"],
        vec!["koshi", "resize-pane", "--direction", "left"],
        vec!["koshi", "toggle-pane-fullscreen"],
        vec!["koshi", "input", "echo hi"],
        vec!["koshi", "new-tab"],
        vec!["koshi", "close-tab"],
        vec!["koshi", "focus-tab", "--index", "1"],
        vec!["koshi", "move-tab", "--index", "1"],
        vec!["koshi", "next-tab"],
        vec!["koshi", "previous-tab"],
        vec!["koshi", "focus-pane", "--pane", &pane],
        vec!["koshi", "lock"],
        vec!["koshi", "unlock"],
        vec!["koshi", "toggle-lock"],
        vec!["koshi", "run", "--", "htop"],
    ];

    let mut named = BTreeSet::new();
    for argv in &action_verbs {
        let command = parse(argv).command.expect("an action verb parses");
        let (action, _) = command
            .to_action(&ResolvedTargets::default(), Direction::Right)
            .unwrap_or_else(|| panic!("{argv:?} maps to an action"));
        assert!(
            registered.contains(&action.to_string()),
            "{argv:?} names {action}, which core_action_seeds does not register"
        );
        named.insert(action.to_string());
    }
    assert_eq!(named.len(), 16, "each verb names its own action");
}
