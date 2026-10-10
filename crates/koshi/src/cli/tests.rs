//! Parse tests for the `koshi` command-line grammar: the bare interactive
//! launch, the headless launch, lifecycle commands, the typed action
//! subcommands and their command mapping, and usage-error diagnostics.

use clap::error::ErrorKind;
use clap::CommandFactory;
use clap::Parser;
use koshi_config::app_config::parse_app_config;
use koshi_core::action::{build_core_action_seeds, ActionHandlerReference};
use std::path::Path;

use super::*;
use uuid::Uuid;

fn parse_cli_arguments(argument_values: &[&str]) -> Cli {
    Cli::try_parse_from(argument_values).expect("argument values must parse")
}

fn parse_cli_error(argument_values: &[&str]) -> clap::Error {
    Cli::try_parse_from(argument_values).expect_err("argument values must fail to parse")
}

/// The parsed subcommand of `argument_values`.
fn parse_cli_command(argument_values: &[&str]) -> CliCommand {
    parse_cli_arguments(argument_values)
        .command
        .expect("argument values must carry a subcommand")
}

/// The `(action, command)` pair the subcommand of `argument_values` maps to,
/// for a CLI with no `koshi.kdl` — its `layout.new-pane-direction` is the
/// built-in `Right`.
fn build_cli_action(argument_values: &[&str]) -> (ActionReference, Command) {
    build_cli_action_for_direction(argument_values, Direction::Right)
}

/// [`build_cli_action`] for a CLI whose own `layout.new-pane-direction` is
/// `new_pane_direction`.
fn build_cli_action_for_direction(
    argument_values: &[&str],
    new_pane_direction: Direction,
) -> (ActionReference, Command) {
    parse_cli_command(argument_values)
        .build_action_command(&ResolvedTargets::default(), new_pane_direction)
        .expect("argument values must map to an action")
}

/// A fixed UUID so id-carrying asserts are exact.
fn build_fixed_test_uuid() -> Uuid {
    Uuid::parse_str("0192f0c1-2345-7000-8000-000000000001").expect("literal UUID is valid")
}

#[test]
fn bare_koshi_is_the_interactive_launch() {
    let cli = parse_cli_arguments(&["koshi"]);
    assert_eq!(
        cli,
        Cli {
            is_headless: false,
            should_allow_other_users: false,
            profile_name: None,
            remote_server_reference: None,
            command: None,
        }
    );
    assert!(cli.is_interactive_launch());
}

#[test]
fn profile_names_a_launch_profile_and_stays_an_interactive_launch() {
    let cli = parse_cli_arguments(&["koshi", "--profile", "dev"]);
    assert_eq!(cli.profile_name, Some("dev".to_string()));
    assert!(cli.is_interactive_launch());
}

#[test]
fn headless_creates_a_session_without_the_interactive_launch() {
    let cli = parse_cli_arguments(&["koshi", "--headless"]);
    assert_eq!(
        cli,
        Cli {
            is_headless: true,
            should_allow_other_users: false,
            profile_name: None,
            remote_server_reference: None,
            command: None,
        }
    );
    assert!(!cli.is_interactive_launch());
}

#[test]
fn headless_takes_the_other_users_flag_beside_it() {
    let cli = parse_cli_arguments(&["koshi", "--headless", "--allow-other-users"]);
    assert_eq!(
        cli,
        Cli {
            is_headless: true,
            should_allow_other_users: true,
            profile_name: None,
            remote_server_reference: None,
            command: None,
        }
    );
}

#[test]
fn the_other_users_flag_without_headless_is_a_usage_error() {
    // `--allow-other-users` requires `--headless`.
    let cli_error = parse_cli_error(&["koshi", "--allow-other-users"]);

    assert_eq!(cli_error.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn attach_without_a_session_picks_one_at_runtime() {
    let cli = parse_cli_arguments(&["koshi", "attach"]);
    assert_eq!(
        cli,
        Cli {
            is_headless: false,
            should_allow_other_users: false,
            profile_name: None,
            remote_server_reference: None,
            command: Some(CliCommand::Attach {
                session_argument: None,
                save_as: None,
            }),
        }
    );
}

#[test]
fn attach_takes_the_session_as_a_positional() {
    let session_id_text = format!("session-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_arguments(&["koshi", "attach", &session_id_text]).command,
        Some(CliCommand::Attach {
            session_argument: Some(session_id_text),
            save_as: None,
        })
    );
}

#[test]
fn attach_takes_a_server_and_the_name_to_save_it_under() {
    let cli = parse_cli_arguments(&[
        "koshi",
        "attach",
        "--remote",
        "laptop.local:7654",
        "--save-as",
        "work",
        "web",
    ]);
    assert_eq!(
        cli.remote_server_reference,
        Some("laptop.local:7654".to_string())
    );
    assert_eq!(
        cli.command,
        Some(CliCommand::Attach {
            session_argument: Some("web".to_string()),
            save_as: Some("work".to_string()),
        })
    );
}

#[test]
fn a_name_to_save_a_server_under_without_a_server_is_a_usage_error() {
    // `--save-as` is declared `requires = "remote"`.
    let cli_error = parse_cli_error(&["koshi", "attach", "--save-as", "work"]);

    assert_eq!(cli_error.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn the_server_flag_reaches_the_action_verbs_and_the_bare_invocation() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "new-pane", "--remote", "work"]).remote_server_reference,
        Some("work".to_string())
    );
    assert_eq!(
        parse_cli_arguments(&["koshi", "close-pane", "--remote", "work"]).remote_server_reference,
        Some("work".to_string())
    );
    // A bare invocation parses here; dispatch refuses it.
    let bare_invocation = parse_cli_arguments(&["koshi", "--remote", "work"]);
    assert_eq!(
        bare_invocation.remote_server_reference,
        Some("work".to_string())
    );
    assert_eq!(bare_invocation.command, None);
}

#[test]
fn bare_detach_names_no_target_and_no_session() {
    let cli = parse_cli_arguments(&["koshi", "detach"]);
    assert_eq!(
        cli,
        Cli {
            is_headless: false,
            should_allow_other_users: false,
            profile_name: None,
            remote_server_reference: None,
            command: Some(CliCommand::Detach {
                detach_target: None,
                should_detach_all_clients: false,
            }),
        }
    );
}

#[test]
fn detach_takes_the_client_as_a_positional() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "detach", "3f2a"]).command,
        Some(CliCommand::Detach {
            detach_target: Some("3f2a".to_string()),
            should_detach_all_clients: false,
        })
    );
}

#[test]
fn detach_all_without_a_session_names_no_target() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "detach", "--all"]).command,
        Some(CliCommand::Detach {
            detach_target: None,
            should_detach_all_clients: true,
        })
    );
}

#[test]
fn detach_all_takes_the_session_as_a_positional() {
    let session_id_text = format!("session-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_arguments(&["koshi", "detach", "--all", &session_id_text]).command,
        Some(CliCommand::Detach {
            detach_target: Some(session_id_text),
            should_detach_all_clients: true,
        })
    );
}

#[test]
fn the_removed_attach_and_detach_root_flags_are_usage_errors() {
    for argument_values in [
        ["koshi", "--attach", "x"].as_slice(),
        ["koshi", "--detach"].as_slice(),
        ["koshi", "--detach-all"].as_slice(),
    ] {
        let cli_error = parse_cli_error(argument_values);
        assert_eq!(cli_error.kind(), ErrorKind::UnknownArgument);
        assert_eq!(cli_error.exit_code(), 2);
    }
}

#[test]
fn headless_conflicts_with_subcommands() {
    let cli_error = parse_cli_error(&["koshi", "--headless", "list-sessions"]);
    assert_eq!(cli_error.kind(), ErrorKind::ArgumentConflict);
}

#[test]
fn the_removed_new_verb_is_a_usage_error() {
    let cli_error = parse_cli_error(&["koshi", "new"]);
    assert_eq!(cli_error.kind(), ErrorKind::InvalidSubcommand);
    assert_eq!(cli_error.exit_code(), 2);
}

#[test]
fn lifecycle_commands_parse() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "list-sessions"]).command,
        Some(CliCommand::ListSessions {
            output_format: OutputFormat::Table
        })
    );
    assert_eq!(
        parse_cli_arguments(&["koshi", "doctor"]).command,
        Some(CliCommand::Doctor {
            output_format: OutputFormat::Table
        })
    );
}

#[test]
fn doctor_takes_a_format() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "doctor", "--format", "json"]).command,
        Some(CliCommand::Doctor {
            output_format: OutputFormat::Json
        })
    );
}

#[test]
fn a_subcommand_is_not_the_interactive_launch() {
    assert!(!parse_cli_arguments(&["koshi", "list-sessions"]).is_interactive_launch());
}

#[test]
fn kill_session_takes_an_optional_session() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "kill-session"]).command,
        Some(CliCommand::KillSession {
            session_reference: None,
        })
    );
    assert_eq!(
        parse_cli_arguments(&["koshi", "kill-session", "work"]).command,
        Some(CliCommand::KillSession {
            session_reference: Some(SessionReference::SessionName("work".to_string()))
        })
    );
}

#[test]
fn kill_session_rejects_a_second_positional() {
    let cli_error = parse_cli_error(&["koshi", "kill-session", "work", "extra"]);
    assert_eq!(cli_error.kind(), ErrorKind::UnknownArgument);
}

#[test]
fn flagless_subcommands_parse_to_their_variants() {
    let cases: &[(&str, CliCommand)] = &[
        (
            "toggle-pane-fullscreen",
            CliCommand::TogglePaneFullscreen { client_id: None },
        ),
        (
            "new-tab",
            CliCommand::NewTab {
                session_reference: None,
                client_id: None,
            },
        ),
        ("next-tab", CliCommand::NextTab { client_id: None }),
        ("previous-tab", CliCommand::PreviousTab { client_id: None }),
        ("lock", CliCommand::Lock { client_id: None }),
        ("unlock", CliCommand::Unlock { client_id: None }),
        ("toggle-lock", CliCommand::ToggleLock { client_id: None }),
        (
            "list-tabs",
            CliCommand::ListTabs {
                session_reference: None,
                output_format: OutputFormat::Table,
            },
        ),
        (
            "list-panes",
            CliCommand::ListPanes {
                session_reference: None,
                output_format: OutputFormat::Table,
            },
        ),
        (
            "list-clients",
            CliCommand::ListClients {
                session_reference: None,
                output_format: OutputFormat::Table,
            },
        ),
    ];
    for (command_name, expected_command) in cases {
        assert_eq!(
            parse_cli_arguments(&["koshi", command_name])
                .command
                .as_ref(),
            Some(expected_command)
        );
    }
}

#[test]
fn config_subcommands_parse_without_a_default_command() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "config", "path"]).command,
        Some(CliCommand::Config {
            command: ConfigCommand::Path,
        })
    );
    assert_eq!(
        parse_cli_arguments(&["koshi", "config", "explain", "koshi.pane.min-cols"]).command,
        Some(CliCommand::Config {
            command: ConfigCommand::Explain {
                config_key: "koshi.pane.min-cols".to_string(),
            },
        })
    );
    assert_eq!(
        parse_cli_arguments(&["koshi", "config", "check"]).command,
        Some(CliCommand::Config {
            command: ConfigCommand::Check,
        })
    );
    assert_eq!(
        parse_cli_arguments(&["koshi", "config", "migrate"]).command,
        Some(CliCommand::Config {
            command: ConfigCommand::Migrate,
        })
    );
    assert_eq!(
        parse_cli_error(&["koshi", "config"]).kind(),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    assert_eq!(
        parse_cli_error(&["koshi", "config", "default"]).kind(),
        ErrorKind::InvalidSubcommand
    );
}

// --- Debug subcommands ---

#[test]
fn dump_state_defaults_to_the_table_format() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "debug", "dump-state"]).command,
        Some(CliCommand::Debug {
            command: DebugCommand::DumpState {
                output_format: OutputFormat::Table,
            },
        })
    );
}

#[test]
fn dump_state_takes_the_json_format() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "debug", "dump-state", "--format", "json"]).command,
        Some(CliCommand::Debug {
            command: DebugCommand::DumpState {
                output_format: OutputFormat::Json,
            },
        })
    );
}

#[test]
fn dump_layout_defaults_to_every_tab_and_the_table_format() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "debug", "dump-layout"]).command,
        Some(CliCommand::Debug {
            command: DebugCommand::DumpLayout {
                tab_reference: None,
                output_format: OutputFormat::Table,
            },
        })
    );
}

#[test]
fn dump_layout_takes_a_tab_id() {
    let tab_id_text = format!("tab-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_arguments(&["koshi", "debug", "dump-layout", "--tab", &tab_id_text]).command,
        Some(CliCommand::Debug {
            command: DebugCommand::DumpLayout {
                tab_reference: Some(TabReference::TabId(TabId::from_uuid(
                    build_fixed_test_uuid()
                ))),
                output_format: OutputFormat::Table,
            },
        })
    );
}

#[test]
fn dump_layout_takes_a_tab_name() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "debug", "dump-layout", "--tab", "editor"]).command,
        Some(CliCommand::Debug {
            command: DebugCommand::DumpLayout {
                tab_reference: Some(TabReference::TabName("editor".to_string())),
                output_format: OutputFormat::Table,
            },
        })
    );
}

#[test]
fn dump_layout_takes_a_tab_and_the_json_format_together() {
    assert_eq!(
        parse_cli_arguments(&[
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
                tab_reference: Some(TabReference::TabName("editor".to_string())),
                output_format: OutputFormat::Json,
            },
        })
    );
}

#[test]
fn debug_events_defaults_to_every_event_and_the_table_format() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "debug", "events"]).command,
        Some(CliCommand::Debug {
            command: DebugCommand::Events {
                event_age_limit: None,
                event_name_filter: None,
                output_format: OutputFormat::Table,
            },
        })
    );
}

#[test]
fn debug_events_takes_a_window_a_filter_and_the_json_format_together() {
    assert_eq!(
        parse_cli_arguments(&[
            "koshi", "debug", "events", "--since", "5m", "--filter", "pane", "--format", "json",
        ])
        .command,
        Some(CliCommand::Debug {
            command: DebugCommand::Events {
                event_age_limit: Some(Duration::from_secs(300)),
                event_name_filter: Some("pane".to_string()),
                output_format: OutputFormat::Json,
            },
        })
    );
}

#[test]
fn debug_events_reads_every_length_unit() {
    for (since_text, expected_second_count) in
        [("45s", 45), ("5m", 300), ("2h", 7200), ("7d", 604_800)]
    {
        let argument_values = ["koshi", "debug", "events", "--since", since_text];
        assert_eq!(
            parse_cli_arguments(&argument_values).command,
            Some(CliCommand::Debug {
                command: DebugCommand::Events {
                    event_age_limit: Some(Duration::from_secs(expected_second_count)),
                    event_name_filter: None,
                    output_format: OutputFormat::Table,
                },
            }),
            "--since {since_text}"
        );
    }
}

#[test]
fn debug_events_refuses_an_empty_filter() {
    assert_eq!(
        parse_cli_error(&["koshi", "debug", "events", "--filter", ""]).kind(),
        ErrorKind::ValueValidation
    );
}

#[test]
fn debug_events_refuses_a_window_it_cannot_read() {
    for since_text in [
        "",
        "5",
        "5w",
        "five minutes",
        "never",
        "18446744073709551615d",
    ] {
        assert_eq!(
            parse_cli_error(&["koshi", "debug", "events", "--since", since_text]).kind(),
            ErrorKind::ValueValidation,
            "--since {since_text}"
        );
    }
    // A leading `-` reaches the parser only when the value is attached, since
    // clap reads a detached one as another flag.
    assert_eq!(
        parse_cli_error(&["koshi", "debug", "events", "--since=-5s"]).kind(),
        ErrorKind::ValueValidation
    );
}

#[test]
fn bare_debug_requires_a_subcommand() {
    assert_eq!(
        parse_cli_error(&["koshi", "debug"]).kind(),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
}

#[test]
fn an_unknown_debug_subcommand_is_refused() {
    assert_eq!(
        parse_cli_error(&["koshi", "debug", "dump-everything"]).kind(),
        ErrorKind::InvalidSubcommand
    );
}

#[test]
fn an_unknown_dump_format_is_refused() {
    assert_eq!(
        parse_cli_error(&["koshi", "debug", "dump-state", "--format", "yaml"]).kind(),
        ErrorKind::InvalidValue
    );
}

#[test]
fn the_debug_dumps_are_queries_and_map_to_no_action() {
    for argument_values in [
        vec!["koshi", "debug", "dump-state"],
        vec!["koshi", "debug", "dump-layout"],
        vec!["koshi", "debug", "events"],
    ] {
        assert_eq!(
            parse_cli_command(&argument_values)
                .build_action_command(&ResolvedTargets::default(), Direction::Right),
            None,
            "{argument_values:?} must stay a read-only query",
        );
    }
}

// --- Keys subcommands ---

#[test]
fn bare_keys_requires_a_subcommand() {
    let cli_error = parse_cli_error(&["koshi", "keys"]);
    assert_eq!(
        cli_error.kind(),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
}

#[test]
fn keys_list_parses_its_filters_and_format() {
    assert_eq!(
        parse_cli_command(&["koshi", "keys", "list"]),
        CliCommand::Keys {
            command: KeysCommand::List {
                input_mode_name: None,
                scope: None,
                output_format: OutputFormat::Table,
            }
        }
    );
    assert_eq!(
        parse_cli_command(&[
            "koshi", "keys", "list", "--mode", "locked", "--scope", "user", "--format", "json",
        ]),
        CliCommand::Keys {
            command: KeysCommand::List {
                input_mode_name: Some("locked".to_string()),
                scope: Some(KeymapScope::User),
                output_format: OutputFormat::Json,
            }
        }
    );
}

#[test]
fn keys_describe_parses_the_sequence() {
    assert_eq!(
        parse_cli_command(&["koshi", "keys", "describe", "<C-p> n"]),
        CliCommand::Keys {
            command: KeysCommand::Describe {
                key_sequence_text: "<C-p> n".to_string(),
                output_format: OutputFormat::Table,
            }
        }
    );
}

#[test]
fn keys_conflicts_parses_a_format() {
    assert_eq!(
        parse_cli_command(&["koshi", "keys", "conflicts", "--format", "json"]),
        CliCommand::Keys {
            command: KeysCommand::Conflicts {
                output_format: OutputFormat::Json,
            }
        }
    );
}

#[test]
fn keys_validate_parses_the_path() {
    assert_eq!(
        parse_cli_command(&["koshi", "keys", "validate", "my-keys.kdl"]),
        CliCommand::Keys {
            command: KeysCommand::Validate {
                keybinding_file_path: PathBuf::from("my-keys.kdl"),
                output_format: OutputFormat::Table,
            }
        }
    );
}

#[test]
fn keys_mutation_verbs_do_not_exist() {
    // Keybindings mutate through `keybinding.kdl` only; the `keys` tree is
    // read-only introspection.
    for verb in ["set", "remove", "reset"] {
        let cli_error = parse_cli_error(&["koshi", "keys", verb]);
        assert_eq!(cli_error.kind(), ErrorKind::InvalidSubcommand, "for {verb}");
    }
}

#[test]
fn keys_queries_map_to_no_action() {
    for argument_values in [
        vec!["koshi", "keys", "list"],
        vec!["koshi", "keys", "describe", "<C-y>"],
        vec!["koshi", "keys", "conflicts"],
        vec!["koshi", "keys", "validate", "f.kdl"],
    ] {
        assert_eq!(
            parse_cli_command(&argument_values)
                .build_action_command(&ResolvedTargets::default(), Direction::Right),
            None
        );
    }
}

// --- Discovery queries ---

#[test]
fn list_tabs_parses_a_typed_session_and_a_format() {
    let session_id_text = format!("session-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_arguments(&[
            "koshi",
            "list-tabs",
            "--session",
            &session_id_text,
            "--format",
            "json"
        ])
        .command,
        Some(CliCommand::ListTabs {
            session_reference: Some(SessionReference::SessionId(SessionId::from_uuid(
                build_fixed_test_uuid()
            ))),
            output_format: OutputFormat::Json,
        })
    );
}

#[test]
fn list_panes_parses_a_session_filter() {
    let session_id_text = format!("session-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_arguments(&["koshi", "list-panes", "--session", &session_id_text]).command,
        Some(CliCommand::ListPanes {
            session_reference: Some(SessionReference::SessionId(SessionId::from_uuid(
                build_fixed_test_uuid()
            ))),
            output_format: OutputFormat::Table,
        })
    );
}

#[test]
fn list_panes_takes_no_tab_filter() {
    let tab_id_text = format!("tab-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_error(&["koshi", "list-panes", "--tab", &tab_id_text]).kind(),
        ErrorKind::UnknownArgument
    );
}

#[test]
fn list_sessions_parses_the_json_format() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "list-sessions", "--format", "json"]).command,
        Some(CliCommand::ListSessions {
            output_format: OutputFormat::Json,
        })
    );
}

#[test]
fn format_rejects_an_unknown_value() {
    let cli_error = parse_cli_error(&["koshi", "list-sessions", "--format", "yaml"]);
    assert_eq!(cli_error.kind(), ErrorKind::InvalidValue);
}

#[test]
fn inspect_forms_parse_typed_ids() {
    let uuid = build_fixed_test_uuid();
    let inspect_cases: &[(&str, String, InspectTarget)] = &[
        (
            "session",
            format!("session-{uuid}"),
            InspectTarget::Session {
                session_reference: SessionReference::SessionId(SessionId::from_uuid(uuid)),
                output_format: OutputFormat::Table,
            },
        ),
        (
            "tab",
            format!("tab-{uuid}"),
            InspectTarget::Tab {
                tab_reference: TabReference::TabId(TabId::from_uuid(uuid)),
                output_format: OutputFormat::Table,
            },
        ),
        (
            "pane",
            format!("pane-{uuid}"),
            InspectTarget::Pane {
                pane_id: PaneId::from_uuid(uuid),
                output_format: OutputFormat::Table,
            },
        ),
        (
            "client",
            format!("client-{uuid}"),
            InspectTarget::Client {
                client_id: ClientId::from_uuid(uuid),
                output_format: OutputFormat::Table,
            },
        ),
    ];
    for (inspect_kind, inspect_target_text, expected_inspect_target) in inspect_cases {
        let command = parse_cli_command(&["koshi", "inspect", inspect_kind, inspect_target_text]);
        let CliCommand::Inspect { inspect_target } = command else {
            panic!("expected an inspect command for {inspect_kind}, got {command:?}");
        };
        assert_eq!(
            &inspect_target, expected_inspect_target,
            "for {inspect_kind}"
        );
    }
}

#[test]
fn inspect_parses_the_json_format() {
    let pane_id_text = format!("pane-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_arguments(&[
            "koshi",
            "inspect",
            "pane",
            &pane_id_text,
            "--format",
            "json"
        ])
        .command,
        Some(CliCommand::Inspect {
            inspect_target: InspectTarget::Pane {
                pane_id: PaneId::from_uuid(build_fixed_test_uuid()),
                output_format: OutputFormat::Json,
            }
        })
    );
}

#[test]
fn inspect_requires_a_target() {
    let cli_error = parse_cli_error(&["koshi", "inspect"]);
    assert_eq!(
        cli_error.kind(),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    assert_eq!(cli_error.exit_code(), 2);
}

#[test]
fn inspect_rejects_an_id_of_the_wrong_kind() {
    let tab_id_text = format!("tab-{}", build_fixed_test_uuid());
    let cli_error = parse_cli_error(&["koshi", "inspect", "pane", &tab_id_text]);
    assert_eq!(cli_error.kind(), ErrorKind::ValueValidation);
}

// --- Action introspection ---

#[test]
fn actions_list_parses_with_a_default_and_a_json_format() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "actions", "list"]).command,
        Some(CliCommand::Actions {
            command: ActionsCommand::List {
                output_format: OutputFormat::Table,
            },
        })
    );
    assert_eq!(
        parse_cli_arguments(&["koshi", "actions", "list", "--format", "json"]).command,
        Some(CliCommand::Actions {
            command: ActionsCommand::List {
                output_format: OutputFormat::Json,
            },
        })
    );
}

#[test]
fn actions_explain_takes_an_action_name_and_a_format() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "actions", "explain", "new-pane"]).command,
        Some(CliCommand::Actions {
            command: ActionsCommand::Explain {
                action_reference_text: "new-pane".to_string(),
                output_format: OutputFormat::Table,
            },
        })
    );
    assert_eq!(
        parse_cli_arguments(&[
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
                action_reference_text: "core:new-pane".to_string(),
                output_format: OutputFormat::Json,
            },
        })
    );
}

#[test]
fn actions_requires_a_subcommand() {
    let cli_error = parse_cli_error(&["koshi", "actions"]);
    assert_eq!(
        cli_error.kind(),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    assert_eq!(cli_error.exit_code(), 2);
}

#[test]
fn actions_explain_requires_an_action() {
    let cli_error = parse_cli_error(&["koshi", "actions", "explain"]);
    assert_eq!(cli_error.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn the_command_tree_lists_exactly_the_declared_subcommands() {
    let mut subcommand_names: Vec<String> = Cli::command()
        .get_subcommands()
        .map(|command| command.get_name().to_string())
        .collect();
    subcommand_names.sort();
    let mut expected_subcommand_names: Vec<String> = [
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
        "move-pane",
        "move-tab",
        "new-pane",
        "new-tab",
        "next-tab",
        "place-pane",
        "previous-tab",
        "remote",
        "resize-pane",
        "restart-servers",
        "resume-support",
        "run",
        "scroll-pane",
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
    expected_subcommand_names.sort();
    assert_eq!(subcommand_names, expected_subcommand_names);
}

#[test]
fn serve_router_takes_the_wait_for_lock_flag() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "serve-router", "--runtime-dir", "X"]).command,
        Some(CliCommand::ServeRouter {
            runtime_directory: Some(PathBuf::from("X")),
            should_wait_for_lock: false,
        })
    );
    assert_eq!(
        parse_cli_arguments(&[
            "koshi",
            "serve-router",
            "--runtime-dir",
            "X",
            "--wait-for-lock"
        ])
        .command,
        Some(CliCommand::ServeRouter {
            runtime_directory: Some(PathBuf::from("X")),
            should_wait_for_lock: true,
        })
    );
    assert_eq!(
        parse_cli_arguments(&["koshi", "serve-router"]).command,
        Some(CliCommand::ServeRouter {
            runtime_directory: None,
            should_wait_for_lock: false,
        })
    );
}

#[test]
fn serve_session_parses_its_two_positionals_and_every_flag() {
    let session_id_text = format!("session-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&["koshi", "serve-session", &session_id_text, "amber-fox"]),
        CliCommand::ServeSession {
            session_id: SessionId::from_uuid(build_fixed_test_uuid()),
            session_name: "amber-fox".to_string(),
            runtime_directory: None,
            profile_name: None,
            should_allow_other_users: false,
            resume_state_path: None,
            supervisor_token: None,
            supervisor_pid: None,
        }
    );
    assert_eq!(
        parse_cli_command(&[
            "koshi",
            "serve-session",
            &session_id_text,
            "amber-fox",
            "--runtime-dir",
            "R",
            "--profile",
            "dev",
            "--allow-other-users",
            "--resume",
            "image.bin",
            "--supervisor-token",
            "secret",
            "--supervisor-pid",
            "4321",
        ]),
        CliCommand::ServeSession {
            session_id: SessionId::from_uuid(build_fixed_test_uuid()),
            session_name: "amber-fox".to_string(),
            runtime_directory: Some(PathBuf::from("R")),
            profile_name: Some("dev".to_string()),
            should_allow_other_users: true,
            resume_state_path: Some(PathBuf::from("image.bin")),
            supervisor_token: Some("secret".to_string()),
            supervisor_pid: Some(4321),
        }
    );
}

#[test]
fn serve_session_requires_both_positionals_and_a_session_id() {
    let session_id_text = format!("session-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_error(&["koshi", "serve-session"]).kind(),
        ErrorKind::MissingRequiredArgument
    );
    assert_eq!(
        parse_cli_error(&["koshi", "serve-session", &session_id_text]).kind(),
        ErrorKind::MissingRequiredArgument
    );
    let tab_id_text = format!("tab-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_error(&["koshi", "serve-session", &tab_id_text, "amber-fox"]).kind(),
        ErrorKind::ValueValidation
    );
}

#[test]
fn serve_session_supervisor_pid_takes_the_u32_range_and_nothing_past_it() {
    let session_id_text = format!("session-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&[
            "koshi",
            "serve-session",
            &session_id_text,
            "amber-fox",
            "--supervisor-pid",
            "4294967295",
        ]),
        CliCommand::ServeSession {
            session_id: SessionId::from_uuid(build_fixed_test_uuid()),
            session_name: "amber-fox".to_string(),
            runtime_directory: None,
            profile_name: None,
            should_allow_other_users: false,
            resume_state_path: None,
            supervisor_token: None,
            supervisor_pid: Some(u32::MAX),
        }
    );
    assert_eq!(
        parse_cli_error(&[
            "koshi",
            "serve-session",
            &session_id_text,
            "amber-fox",
            "--supervisor-pid",
            "4294967296",
        ])
        .kind(),
        ErrorKind::ValueValidation
    );
}

#[test]
fn serve_pty_supervisor_parses_its_session_token_and_runtime_dir() {
    let session_id_text = format!("session-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&["koshi", "serve-pty-supervisor", &session_id_text, "secret"]),
        CliCommand::ServePtySupervisor {
            session_id: SessionId::from_uuid(build_fixed_test_uuid()),
            supervisor_token: "secret".to_string(),
            runtime_directory: None,
        }
    );
    assert_eq!(
        parse_cli_command(&[
            "koshi",
            "serve-pty-supervisor",
            &session_id_text,
            "secret",
            "--runtime-dir",
            "R",
        ]),
        CliCommand::ServePtySupervisor {
            session_id: SessionId::from_uuid(build_fixed_test_uuid()),
            supervisor_token: "secret".to_string(),
            runtime_directory: Some(PathBuf::from("R")),
        }
    );
    assert_eq!(
        parse_cli_error(&["koshi", "serve-pty-supervisor", &session_id_text]).kind(),
        ErrorKind::MissingRequiredArgument
    );
}

#[test]
fn the_argument_free_verbs_take_no_arguments() {
    assert_eq!(
        parse_cli_command(&["koshi", "resume-support"]),
        CliCommand::ResumeSupport
    );
    assert_eq!(parse_cli_command(&["koshi", "update"]), CliCommand::Update);
    assert_eq!(
        parse_cli_command(&["koshi", "restart-servers"]),
        CliCommand::RestartServers
    );
    assert_eq!(
        parse_cli_error(&["koshi", "restart-servers", "--all"]).kind(),
        ErrorKind::UnknownArgument
    );
    assert_eq!(
        parse_cli_error(&["koshi", "resume-support", "extra"]).kind(),
        ErrorKind::UnknownArgument
    );
    assert_eq!(
        parse_cli_error(&["koshi", "update", "--now"]).kind(),
        ErrorKind::UnknownArgument
    );
}

#[test]
fn the_grammar_takes_the_verbs_the_spawners_name() {
    let cli_grammar = Cli::command();
    let subcommands: Vec<&str> = cli_grammar
        .get_subcommands()
        .map(clap::Command::get_name)
        .collect();

    for spawned_verb in [
        koshi_link::router_client::ROUTER_SUBCOMMAND,
        koshi_daemon::pty_supervisor::PTY_SUPERVISOR_SUBCOMMAND,
        koshi_daemon::session_server::SESSION_SERVER_SUBCOMMAND,
        koshi_daemon::session_server::RESUME_SUPPORT_SUBCOMMAND,
    ] {
        assert!(
            subcommands.contains(&spawned_verb),
            "a spawner runs `koshi {spawned_verb}`, and the grammar has no such verb: {subcommands:?}"
        );
    }
}

#[test]
fn the_help_hides_the_self_run_subcommands() {
    let hidden_subcommand_names: Vec<String> = Cli::command()
        .get_subcommands()
        .filter(|command| command.is_hide_set())
        .map(|command| command.get_name().to_string())
        .collect();

    assert_eq!(
        hidden_subcommand_names,
        [
            "serve-router",
            "serve-session",
            "serve-pty-supervisor",
            "resume-support"
        ]
    );
}

#[test]
fn an_unknown_subcommand_is_a_usage_error() {
    let cli_error = parse_cli_error(&["koshi", "explode"]);
    assert_eq!(cli_error.kind(), ErrorKind::InvalidSubcommand);
    assert_eq!(cli_error.exit_code(), 2);
}

#[test]
fn an_unknown_flag_is_a_usage_error() {
    let cli_error = parse_cli_error(&["koshi", "--frobnicate"]);
    assert_eq!(cli_error.kind(), ErrorKind::UnknownArgument);
    assert_eq!(cli_error.exit_code(), 2);
}

#[test]
fn help_and_version_display_and_exit_zero() {
    let help_display = parse_cli_error(&["koshi", "--help"]);
    assert_eq!(help_display.kind(), ErrorKind::DisplayHelp);
    assert_eq!(help_display.exit_code(), 0);

    let version_display = parse_cli_error(&["koshi", "--version"]);
    assert_eq!(version_display.kind(), ErrorKind::DisplayVersion);
    assert_eq!(version_display.exit_code(), 0);
}

#[test]
fn every_subcommand_answers_help() {
    for subcommand_name in Cli::command()
        .get_subcommands()
        .map(|command| command.get_name().to_string())
        .collect::<Vec<_>>()
    {
        let cli_error = parse_cli_error(&["koshi", &subcommand_name, "--help"]);
        assert_eq!(
            cli_error.kind(),
            ErrorKind::DisplayHelp,
            "for subcommand {subcommand_name}"
        );
    }
}

#[test]
fn an_unknown_flag_prints_the_flag_and_the_usage_lines() {
    assert_eq!(
        parse_cli_error(&["koshi", "--frobnicate"]).to_string(),
        "error: unexpected argument '--frobnicate' found\n\n\
         Usage: koshi [OPTIONS]\n       koshi <COMMAND>\n\n\
         For more information, try '--help'.\n"
    );
}

#[test]
fn an_unknown_subcommand_prints_the_subcommand_and_the_usage_lines() {
    assert_eq!(
        parse_cli_error(&["koshi", "explode"]).to_string(),
        "error: unrecognized subcommand 'explode'\n\n\
         Usage: koshi [OPTIONS]\n       koshi <COMMAND>\n\n\
         For more information, try '--help'.\n"
    );
}

#[test]
fn an_invalid_direction_prints_every_direction_it_accepts() {
    assert_eq!(
        parse_cli_error(&["koshi", "new-pane", "--direction", "sideways"]).to_string(),
        "error: invalid value 'sideways' for '--direction <DIRECTION>'\n  \
         [possible values: right, down, left, up]\n\n\
         For more information, try '--help'.\n"
    );
}

/// `lock` has no `--session` flag; the parser refuses it.
#[test]
fn a_session_target_on_lock_prints_the_flag_and_the_lock_usage_line() {
    assert_eq!(
        parse_cli_error(&["koshi", "lock", "--session", "work"]).to_string(),
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
        parse_cli_error(&["koshi", "resize-pane", "--help"]).to_string(),
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
         --client <CLIENT_ID>\n          \
         Client whose view of a floating pane keeps the edge opposite the moved border in \
         place; defaults to the issuing client, else the session's only attached one\n\n      \
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
        parse_cli_command(&["koshi", "new-pane"]),
        CliCommand::NewPane {
            new_pane_flags: NewPaneFlags {
                direction: None,
                should_stack: false,
                pane_id: None,
                session_reference: None,
                tab_reference: None,
                client_id: None,
            },
        }
    );
    let pane_flag = format!("pane-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&[
            "koshi",
            "new-pane",
            "--direction",
            "right",
            "--pane",
            &pane_flag
        ]),
        CliCommand::NewPane {
            new_pane_flags: NewPaneFlags {
                direction: Some(DirectionArgument::Right),
                should_stack: false,
                pane_id: Some(PaneId::from_uuid(build_fixed_test_uuid())),
                session_reference: None,
                tab_reference: None,
                client_id: None,
            },
        }
    );
    assert_eq!(
        parse_cli_command(&["koshi", "new-pane", "--stacked"]),
        CliCommand::NewPane {
            new_pane_flags: NewPaneFlags {
                direction: None,
                should_stack: true,
                pane_id: None,
                session_reference: None,
                tab_reference: None,
                client_id: None,
            },
        }
    );
}

#[test]
fn new_pane_direction_and_stacked_conflict() {
    let cli_error = parse_cli_error(&["koshi", "new-pane", "--direction", "left", "--stacked"]);
    assert_eq!(cli_error.kind(), ErrorKind::ArgumentConflict);
    assert_eq!(cli_error.exit_code(), 2);
}

/// With no `--direction`, `new-pane` splits toward the direction this
/// machine's `koshi.kdl` names, read by the real parser and the real client
/// fold.
#[test]
fn new_pane_without_a_direction_flag_follows_the_config_file() {
    let config_file = parse_app_config(
        Path::new("koshi.kdl"),
        "version 1\nlayout {\n    new-pane-direction \"down\"\n}\n",
    )
    .expect("the fixture parses");
    let configured_direction =
        koshi_link::config::resolve_new_pane_direction(Some(config_file.layer));
    assert_eq!(
        configured_direction,
        Direction::Down,
        "the file's own value"
    );

    let (_, mapped_command) =
        build_cli_action_for_direction(&["koshi", "new-pane"], configured_direction);
    assert_eq!(
        mapped_command,
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Down,
            },
            working_directory: None,
            spawn_spec: None,
            client_id: None,
        })
    );
}

/// An explicit `--direction` beats the file, and `run` reads the file the same
/// way `new-pane` does.
#[test]
fn an_explicit_direction_flag_wins_over_the_config_file() {
    let (_, mapped_command) = build_cli_action_for_direction(
        &["koshi", "new-pane", "--direction", "left"],
        Direction::Down,
    );
    let Command::NewPane(command_args) = mapped_command else {
        panic!("new-pane maps to NewPane");
    };
    assert_eq!(
        command_args.placement,
        NewPanePlacement::Split {
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Left,
        }
    );

    let (_, mapped_command) =
        build_cli_action_for_direction(&["koshi", "run", "--", "htop"], Direction::Down);
    let Command::NewPane(command_args) = mapped_command else {
        panic!("run maps to NewPane");
    };
    assert_eq!(
        command_args.placement,
        NewPanePlacement::Split {
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Down,
        }
    );
}

/// No config directory, no `koshi.kdl`, or a file that did not parse: the fold
/// leaves the built-in `Right`.
#[test]
fn no_config_file_leaves_the_built_in_split_direction() {
    assert_eq!(
        koshi_link::config::resolve_new_pane_direction(None),
        Direction::Right
    );

    let (_, mapped_command) = build_cli_action(&["koshi", "new-pane"]);
    let Command::NewPane(command_args) = mapped_command else {
        panic!("new-pane maps to NewPane");
    };
    assert_eq!(
        command_args.placement,
        NewPanePlacement::Split {
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Right,
        }
    );
}

#[test]
fn new_pane_parses_session_tab_and_client_targets() {
    let client_flag = format!("client-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&[
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
            new_pane_flags: NewPaneFlags {
                direction: None,
                should_stack: false,
                pane_id: None,
                session_reference: Some(SessionReference::SessionName("amber-fox".to_string())),
                tab_reference: Some(TabReference::TabName("logs".to_string())),
                client_id: Some(ClientId::from_uuid(build_fixed_test_uuid())),
            },
        }
    );
}

#[test]
fn new_pane_tab_given_as_an_id_reaches_the_command_without_a_lookup() {
    // With no resolved targets, a `--tab` id still reaches the command's
    // `tab` field.
    let tab_flag = format!("tab-{}", build_fixed_test_uuid());
    let (_, mapped_command) = build_cli_action(&["koshi", "new-pane", "--tab", &tab_flag]);
    assert_eq!(
        mapped_command,
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: None,
                tab_id: Some(TabId::from_uuid(build_fixed_test_uuid())),
                direction: Direction::Right,
            },
            working_directory: None,
            spawn_spec: None,
            client_id: None,
        })
    );
}

#[test]
fn new_pane_pane_and_tab_conflict() {
    let pane_flag = format!("pane-{}", build_fixed_test_uuid());
    let cli_error = parse_cli_error(&["koshi", "new-pane", "--pane", &pane_flag, "--tab", "logs"]);
    assert_eq!(cli_error.kind(), ErrorKind::ArgumentConflict);
    assert_eq!(cli_error.exit_code(), 2);
}

#[test]
fn lock_verbs_take_an_optional_client() {
    let client_flag = format!("client-{}", build_fixed_test_uuid());
    let client_id = Some(ClientId::from_uuid(build_fixed_test_uuid()));
    assert_eq!(
        parse_cli_command(&["koshi", "lock", "--client", &client_flag]),
        CliCommand::Lock { client_id }
    );
    let (_, mapped_command) = build_cli_action(&["koshi", "lock", "--client", &client_flag]);
    assert_eq!(
        mapped_command,
        Command::SetLockMode(LockModeArgs {
            is_locked: true,
            client_id,
        })
    );
    let (_, mapped_command) = build_cli_action(&["koshi", "unlock", "--client", &client_flag]);
    assert_eq!(
        mapped_command,
        Command::SetLockMode(LockModeArgs {
            is_locked: false,
            client_id,
        })
    );
    let (_, mapped_command) = build_cli_action(&["koshi", "toggle-lock", "--client", &client_flag]);
    assert_eq!(
        mapped_command,
        Command::ToggleLockMode(ToggleLockModeArgs { client_id })
    );
}

#[test]
fn new_tab_takes_an_optional_session_and_client() {
    let client_flag = format!("client-{}", build_fixed_test_uuid());
    let client_id = ClientId::from_uuid(build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&["koshi", "new-tab", "--session", "amber-fox"]),
        CliCommand::NewTab {
            session_reference: Some(SessionReference::SessionName("amber-fox".to_string())),
            client_id: None,
        }
    );
    assert_eq!(
        parse_cli_command(&[
            "koshi",
            "new-tab",
            "--session",
            "amber-fox",
            "--client",
            &client_flag
        ]),
        CliCommand::NewTab {
            session_reference: Some(SessionReference::SessionName("amber-fox".to_string())),
            client_id: Some(client_id),
        }
    );
    assert_eq!(
        parse_cli_command(&["koshi", "new-tab", "--session", "amber-fox"])
            .get_target_session_reference(),
        Some(&SessionReference::SessionName("amber-fox".to_string()))
    );
}

#[test]
fn new_tab_carries_its_client_into_the_command_and_the_routing_target() {
    let client_flag = format!("client-{}", build_fixed_test_uuid());
    let client_id = ClientId::from_uuid(build_fixed_test_uuid());
    let parsed_command = parse_cli_command(&["koshi", "new-tab", "--client", &client_flag]);
    assert_eq!(parsed_command.get_target_client_id(), Some(client_id));
    assert_eq!(parsed_command.get_target_session_reference(), None);
    let (_, mapped_command) = build_cli_action(&["koshi", "new-tab", "--client", &client_flag]);
    assert_eq!(
        mapped_command,
        Command::NewTab(NewTabArgs {
            working_directory: None,
            client_id: Some(client_id),
        })
    );

    // With no flag the command names no client.
    let bare_command = parse_cli_command(&["koshi", "new-tab"]);
    assert_eq!(bare_command.get_target_client_id(), None);
}

#[test]
fn new_tab_client_value_must_read_as_a_client_id() {
    let cli_error = parse_cli_error(&["koshi", "new-tab", "--client", "amber-fox"]);
    assert_eq!(cli_error.kind(), ErrorKind::ValueValidation);
    assert_eq!(cli_error.exit_code(), 2);
}

#[test]
fn toggle_pane_fullscreen_takes_a_client_flag() {
    let client_flag = format!("client-{}", build_fixed_test_uuid());
    let client_id = ClientId::from_uuid(build_fixed_test_uuid());
    let parsed_command =
        parse_cli_command(&["koshi", "toggle-pane-fullscreen", "--client", &client_flag]);
    assert_eq!(
        parsed_command,
        CliCommand::TogglePaneFullscreen {
            client_id: Some(client_id),
        }
    );

    // The flag never reaches the command; it rides on the command's source.
    let (action_reference, mapped_command) = parsed_command
        .build_action_command(&ResolvedTargets::default(), Direction::Right)
        .expect("toggle-pane-fullscreen is an action");
    assert_eq!(
        action_reference,
        ActionReference::from_core_action_name("toggle-pane-fullscreen").expect("valid")
    );
    assert_eq!(mapped_command, Command::TogglePaneFullscreen);
}

#[test]
fn resize_pane_takes_a_client_flag_that_rides_on_the_source() {
    let client_flag = format!("client-{}", build_fixed_test_uuid());
    let client_id = ClientId::from_uuid(build_fixed_test_uuid());
    let parsed_command = parse_cli_command(&[
        "koshi",
        "resize-pane",
        "--direction",
        "right",
        "--size",
        "3",
        "--client",
        &client_flag,
    ]);
    assert_eq!(
        parsed_command,
        CliCommand::ResizePane {
            direction: DirectionArgument::Right,
            resize_amount_cells: 3,
            pane_id: None,
            client_id: Some(client_id),
        }
    );
    assert_eq!(parsed_command.get_target_client_id(), Some(client_id));
    assert_eq!(parsed_command.get_source_client_id(), Some(client_id));

    // The flag never reaches the command; it rides on the command's source.
    let (_, mapped_command) = parsed_command
        .build_action_command(&ResolvedTargets::default(), Direction::Left)
        .expect("resize-pane is an action");
    assert_eq!(
        mapped_command,
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Right,
            resize_amount_cells: 3,
        })
    );
}

#[test]
fn a_fullscreen_client_flag_is_a_routing_target() {
    let client_id = ClientId::from_uuid(build_fixed_test_uuid());
    assert_eq!(
        CliCommand::TogglePaneFullscreen {
            client_id: Some(client_id),
        }
        .get_target_client_id(),
        Some(client_id)
    );
}

#[test]
fn an_invalid_direction_is_a_usage_error() {
    let cli_error = parse_cli_error(&["koshi", "new-pane", "--direction", "sideways"]);
    assert_eq!(cli_error.kind(), ErrorKind::InvalidValue);
    assert_eq!(cli_error.exit_code(), 2);
}

#[test]
fn every_direction_value_parses_to_its_core_direction() {
    let direction_cases: &[(&str, Direction)] = &[
        ("right", Direction::Right),
        ("down", Direction::Down),
        ("left", Direction::Left),
        ("up", Direction::Up),
    ];
    for (direction_text, expected_direction) in direction_cases {
        let (_, mapped_command) =
            build_cli_action(&["koshi", "new-pane", "--direction", direction_text]);
        assert_eq!(
            mapped_command,
            Command::NewPane(NewPaneArgs {
                placement: NewPanePlacement::Split {
                    source_pane_id: None,
                    tab_id: None,
                    direction: *expected_direction,
                },
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            })
        );
    }
}

#[test]
fn close_pane_parses_target_and_force() {
    assert_eq!(
        parse_cli_command(&["koshi", "close-pane"]),
        CliCommand::ClosePane {
            pane_id: None,
            should_force_close: false,
        }
    );
    let pane_flag = format!("pane-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&["koshi", "close-pane", "--pane", &pane_flag, "--force"]),
        CliCommand::ClosePane {
            pane_id: Some(PaneId::from_uuid(build_fixed_test_uuid())),
            should_force_close: true,
        }
    );
}

#[test]
fn resize_pane_defaults_the_size_to_one() {
    assert_eq!(
        parse_cli_command(&["koshi", "resize-pane", "--direction", "left"]),
        CliCommand::ResizePane {
            direction: DirectionArgument::Left,
            resize_amount_cells: 1,
            pane_id: None,
            client_id: None,
        }
    );
}

#[test]
fn resize_pane_accepts_a_negative_size_in_both_spellings() {
    assert_eq!(
        parse_cli_command(&["koshi", "resize-pane", "--direction", "up", "--size", "-3"]),
        CliCommand::ResizePane {
            direction: DirectionArgument::Up,
            resize_amount_cells: -3,
            pane_id: None,
            client_id: None,
        }
    );
    assert_eq!(
        parse_cli_command(&["koshi", "resize-pane", "--direction", "up", "--size=-3"]),
        CliCommand::ResizePane {
            direction: DirectionArgument::Up,
            resize_amount_cells: -3,
            pane_id: None,
            client_id: None,
        }
    );
}

#[test]
fn resize_pane_requires_a_direction() {
    let cli_error = parse_cli_error(&["koshi", "resize-pane", "--size", "2"]);
    assert_eq!(cli_error.kind(), ErrorKind::MissingRequiredArgument);
    assert_eq!(cli_error.exit_code(), 2);
}

#[test]
fn place_pane_requires_a_pane_tab_and_direction() {
    let pane_flag = format!("pane-{}", build_fixed_test_uuid());
    for argument_values in [
        vec![
            "koshi",
            "place-pane",
            "--tab",
            "logs",
            "--direction",
            "left",
        ],
        vec![
            "koshi",
            "place-pane",
            "--pane",
            pane_flag.as_str(),
            "--direction",
            "left",
        ],
        vec![
            "koshi",
            "place-pane",
            "--pane",
            pane_flag.as_str(),
            "--tab",
            "logs",
        ],
    ] {
        assert_eq!(
            parse_cli_error(&argument_values).kind(),
            ErrorKind::MissingRequiredArgument
        );
    }
}

#[test]
fn input_parses_its_text_target_and_enter_flag() {
    assert_eq!(
        parse_cli_command(&["koshi", "input", "ls"]),
        CliCommand::Input {
            input_text: "ls".to_string(),
            pane_id: None,
            should_leave_input_at_prompt: false,
        }
    );
    let pane_flag = format!("pane-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&[
            "koshi",
            "input",
            "--pane",
            &pane_flag,
            "--no-enter",
            "ls -la"
        ]),
        CliCommand::Input {
            input_text: "ls -la".to_string(),
            pane_id: Some(PaneId::from_uuid(build_fixed_test_uuid())),
            should_leave_input_at_prompt: true,
        }
    );
}

/// Text that starts with `-` is text, not a flag: `koshi input "-la"` types
/// `-la`. A real flag still parses as a flag on either side of the text.
#[test]
fn input_takes_text_that_starts_with_a_dash() {
    assert_eq!(
        parse_cli_command(&["koshi", "input", "-la"]),
        CliCommand::Input {
            input_text: "-la".to_string(),
            pane_id: None,
            should_leave_input_at_prompt: false,
        }
    );

    // A flag AFTER the text is still a flag, not more text.
    assert_eq!(
        parse_cli_command(&["koshi", "input", "ls", "--no-enter"]),
        CliCommand::Input {
            input_text: "ls".to_string(),
            pane_id: None,
            should_leave_input_at_prompt: true,
        }
    );
}

#[test]
fn input_requires_its_text() {
    let cli_error = parse_cli_error(&["koshi", "input"]);
    assert_eq!(cli_error.kind(), ErrorKind::MissingRequiredArgument);
}

/// The text travels as typed. A carriage return — the byte the Enter key
/// sends — follows it unless `--no-enter` is given.
#[test]
fn input_appends_enter_unless_no_enter_is_given() {
    let pane_flag = format!("pane-{}", build_fixed_test_uuid());

    let (action_reference, command) =
        build_cli_action(&["koshi", "input", "--pane", &pane_flag, "ls"]);
    assert_eq!(
        action_reference,
        ActionReference::from_core_action_name("write-to-pane").expect("valid")
    );
    assert_eq!(
        command,
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(PaneId::from_uuid(build_fixed_test_uuid())),
            pane_input_bytes: b"ls\r".to_vec(),
        })
    );

    let (_, command) = build_cli_action(&["koshi", "input", "--no-enter", "ls"]);
    assert_eq!(
        command,
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: None,
            pane_input_bytes: b"ls".to_vec(),
        })
    );
}

#[test]
fn close_tab_parses_target_and_force() {
    let tab_flag = format!("tab-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&["koshi", "close-tab", "--tab", &tab_flag, "--force"]),
        CliCommand::CloseTab {
            tab_reference: Some(TabReference::TabId(TabId::from_uuid(
                build_fixed_test_uuid()
            ))),
            session_reference: None,
            should_force_close: true,
        }
    );
    // A value that does not read as a tab id is taken as a tab name.
    assert_eq!(
        parse_cli_command(&["koshi", "close-tab", "--tab", "logs"]),
        CliCommand::CloseTab {
            tab_reference: Some(TabReference::TabName("logs".to_string())),
            session_reference: None,
            should_force_close: false,
        }
    );
}

#[test]
fn move_tab_requires_an_index() {
    assert_eq!(
        parse_cli_command(&["koshi", "move-tab", "--index", "2"]),
        CliCommand::MoveTab {
            tab_index: 2,
            tab_reference: None,
        }
    );
    let cli_error = parse_cli_error(&["koshi", "move-tab"]);
    assert_eq!(cli_error.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn focus_tab_takes_exactly_one_of_index_or_tab() {
    assert_eq!(
        parse_cli_command(&["koshi", "focus-tab", "--index", "1"]),
        CliCommand::FocusTab {
            tab_index: Some(1),
            tab_reference: None,
            client_id: None,
        }
    );
    let tab_flag = format!("tab-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&["koshi", "focus-tab", "--tab", &tab_flag]),
        CliCommand::FocusTab {
            tab_index: None,
            tab_reference: Some(TabReference::TabId(TabId::from_uuid(
                build_fixed_test_uuid()
            ))),
            client_id: None,
        }
    );

    let both_targets_error =
        parse_cli_error(&["koshi", "focus-tab", "--index", "1", "--tab", &tab_flag]);
    assert_eq!(both_targets_error.kind(), ErrorKind::ArgumentConflict);
    let no_target_error = parse_cli_error(&["koshi", "focus-tab"]);
    assert_eq!(no_target_error.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn tab_focus_commands_take_an_optional_client() {
    let client_flag = format!("client-{}", build_fixed_test_uuid());
    let client_id = ClientId::from_uuid(build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&[
            "koshi",
            "focus-tab",
            "--index",
            "1",
            "--client",
            &client_flag
        ]),
        CliCommand::FocusTab {
            tab_index: Some(1),
            tab_reference: None,
            client_id: Some(client_id),
        }
    );
    assert_eq!(
        parse_cli_command(&["koshi", "next-tab", "--client", &client_flag]),
        CliCommand::NextTab {
            client_id: Some(client_id),
        }
    );
    assert_eq!(
        parse_cli_command(&["koshi", "previous-tab", "--client", &client_flag]),
        CliCommand::PreviousTab {
            client_id: Some(client_id),
        }
    );

    // The client rides into the mapped command for all three verbs.
    let (_, mapped_command) = build_cli_action(&["koshi", "next-tab", "--client", &client_flag]);
    assert_eq!(
        mapped_command,
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Next,
            client_id: Some(client_id),
        })
    );
    let (_, mapped_command) = build_cli_action(&[
        "koshi",
        "focus-tab",
        "--tab",
        &format!("tab-{}", build_fixed_test_uuid()),
        "--client",
        &client_flag,
    ]);
    assert_eq!(
        mapped_command,
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(TabId::from_uuid(build_fixed_test_uuid())),
            client_id: Some(client_id),
        })
    );
}

#[test]
fn focus_pane_requires_a_pane_and_takes_an_optional_client() {
    let cli_error = parse_cli_error(&["koshi", "focus-pane"]);
    assert_eq!(cli_error.kind(), ErrorKind::MissingRequiredArgument);

    let pane_flag = format!("pane-{}", build_fixed_test_uuid());
    let client_flag = format!("client-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&[
            "koshi",
            "focus-pane",
            "--pane",
            &pane_flag,
            "--client",
            &client_flag
        ]),
        CliCommand::FocusPane {
            pane_id: PaneId::from_uuid(build_fixed_test_uuid()),
            client_id: Some(ClientId::from_uuid(build_fixed_test_uuid())),
        }
    );
}

#[test]
fn run_takes_its_command_after_the_separator() {
    assert_eq!(
        parse_cli_command(&["koshi", "run", "--", "htop", "-d", "5"]),
        CliCommand::Run {
            new_pane_flags: NewPaneFlags {
                direction: None,
                should_stack: false,
                pane_id: None,
                session_reference: None,
                tab_reference: None,
                client_id: None,
            },
            command_arguments: vec!["htop".to_string(), "-d".to_string(), "5".to_string()],
        }
    );
    assert_eq!(
        parse_cli_command(&["koshi", "run", "--direction", "down", "--", "htop"]),
        CliCommand::Run {
            new_pane_flags: NewPaneFlags {
                direction: Some(DirectionArgument::Down),
                should_stack: false,
                pane_id: None,
                session_reference: None,
                tab_reference: None,
                client_id: None,
            },
            command_arguments: vec!["htop".to_string()],
        }
    );
}

#[test]
fn run_takes_an_optional_source_pane() {
    let pane_flag = format!("pane-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&["koshi", "run", "--pane", &pane_flag, "--", "htop"]),
        CliCommand::Run {
            new_pane_flags: NewPaneFlags {
                direction: None,
                should_stack: false,
                pane_id: Some(PaneId::from_uuid(build_fixed_test_uuid())),
                session_reference: None,
                tab_reference: None,
                client_id: None,
            },
            command_arguments: vec!["htop".to_string()],
        }
    );

    // The source pane rides into the mapped command.
    let (_, mapped_command) =
        build_cli_action(&["koshi", "run", "--pane", &pane_flag, "--", "htop"]);
    assert_eq!(
        mapped_command,
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(PaneId::from_uuid(build_fixed_test_uuid())),
                tab_id: None,
                direction: Direction::Right,
            },
            working_directory: None,
            spawn_spec: Some(SpawnSpec {
                program: PathBuf::from("htop"),
                arguments: vec![],
                working_directory: None,
                environment_variables: BTreeMap::new(),
                shell_kind: ShellKind::Other("htop".to_string()),
            }),
            client_id: None,
        })
    );
}

#[test]
fn run_without_a_command_is_a_usage_error() {
    let bare_run_error = parse_cli_error(&["koshi", "run"]);
    assert_eq!(bare_run_error.kind(), ErrorKind::MissingRequiredArgument);
    let empty_run_error = parse_cli_error(&["koshi", "run", "--"]);
    assert_eq!(empty_run_error.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn run_rejects_a_command_not_behind_the_separator() {
    let cli_error = parse_cli_error(&["koshi", "run", "htop"]);
    assert_eq!(cli_error.kind(), ErrorKind::UnknownArgument);
}

#[test]
fn run_direction_and_stacked_conflict() {
    let cli_error = parse_cli_error(&[
        "koshi",
        "run",
        "--direction",
        "up",
        "--stacked",
        "--",
        "htop",
    ]);
    assert_eq!(cli_error.kind(), ErrorKind::ArgumentConflict);
}

// --- Id parsing ---

#[test]
fn ids_parse_prefixed_and_bare_forms() {
    let bare_uuid_text = build_fixed_test_uuid().to_string();
    assert_eq!(
        parse_cli_command(&["koshi", "close-pane", "--pane", &bare_uuid_text]),
        CliCommand::ClosePane {
            pane_id: Some(PaneId::from_uuid(build_fixed_test_uuid())),
            should_force_close: false,
        }
    );
    let prefixed = format!("pane-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&["koshi", "close-pane", "--pane", &prefixed]),
        CliCommand::ClosePane {
            pane_id: Some(PaneId::from_uuid(build_fixed_test_uuid())),
            should_force_close: false,
        }
    );
}

#[test]
fn an_id_of_the_wrong_kind_is_a_usage_error() {
    let tab_id_text = format!("tab-{}", build_fixed_test_uuid());
    let cli_error = parse_cli_error(&["koshi", "close-pane", "--pane", &tab_id_text]);
    assert_eq!(cli_error.kind(), ErrorKind::ValueValidation);
    assert_eq!(cli_error.exit_code(), 2);
}

#[test]
fn a_malformed_id_is_a_usage_error() {
    let cli_error = parse_cli_error(&["koshi", "close-pane", "--pane", "not-a-uuid"]);
    assert_eq!(cli_error.kind(), ErrorKind::ValueValidation);
    assert_eq!(cli_error.exit_code(), 2);
}

// --- Value parsers, called directly ---

#[test]
fn each_id_parser_takes_its_own_prefix_a_bare_uuid_and_nothing_else() {
    let uuid = build_fixed_test_uuid();
    assert_eq!(
        parse_pane_id(&format!("pane-{uuid}")),
        Ok(PaneId::from_uuid(uuid))
    );
    assert_eq!(
        parse_pane_id(&uuid.to_string()),
        Ok(PaneId::from_uuid(uuid))
    );
    assert_eq!(
        parse_pane_id(&format!("tab-{uuid}")),
        Err("expected `pane-<uuid>` or a bare UUID".to_string())
    );

    assert_eq!(
        parse_client_id(&format!("client-{uuid}")),
        Ok(ClientId::from_uuid(uuid))
    );
    assert_eq!(
        parse_client_id("amber-fox"),
        Err("expected `client-<uuid>` or a bare UUID".to_string())
    );

    assert_eq!(
        parse_session_id(&format!("session-{uuid}")),
        Ok(SessionId::from_uuid(uuid))
    );
    assert_eq!(
        parse_session_id(""),
        Err("expected `session-<uuid>` or a bare UUID".to_string())
    );
}

/// The prefix is stripped once, so a value carrying it twice is neither a
/// prefixed id nor a bare UUID.
#[test]
fn a_doubled_id_prefix_is_refused() {
    let doubled_prefix_pane_id_text = format!("pane-pane-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_pane_id(&doubled_prefix_pane_id_text),
        Err("expected `pane-<uuid>` or a bare UUID".to_string())
    );
}

#[test]
fn parse_session_reference_reads_an_id_a_name_and_refuses_an_empty_value() {
    let uuid = build_fixed_test_uuid();
    assert_eq!(
        parse_session_reference(&format!("session-{uuid}")),
        Ok(SessionReference::SessionId(SessionId::from_uuid(uuid)))
    );
    assert_eq!(
        parse_session_reference(&uuid.to_string()),
        Ok(SessionReference::SessionId(SessionId::from_uuid(uuid)))
    );
    assert_eq!(
        parse_session_reference("work"),
        Ok(SessionReference::SessionName("work".to_string()))
    );
    // Another kind's id does not read as a session id, so it is kept whole as
    // a name.
    let tab_id_text = format!("tab-{uuid}");
    assert_eq!(
        parse_session_reference(&tab_id_text),
        Ok(SessionReference::SessionName(tab_id_text.clone()))
    );
    assert_eq!(
        parse_session_reference(""),
        Err("expected a session id or name".to_string())
    );
}

#[test]
fn parse_tab_reference_reads_an_id_a_name_and_refuses_an_empty_value() {
    let uuid = build_fixed_test_uuid();
    assert_eq!(
        parse_tab_reference(&format!("tab-{uuid}")),
        Ok(TabReference::TabId(TabId::from_uuid(uuid)))
    );
    assert_eq!(
        parse_tab_reference(&uuid.to_string()),
        Ok(TabReference::TabId(TabId::from_uuid(uuid)))
    );
    assert_eq!(
        parse_tab_reference("logs"),
        Ok(TabReference::TabName("logs".to_string()))
    );
    let pane_id_text = format!("pane-{uuid}");
    assert_eq!(
        parse_tab_reference(&pane_id_text),
        Ok(TabReference::TabName(pane_id_text.clone()))
    );
    assert_eq!(
        parse_tab_reference(""),
        Err("expected a tab id or name".to_string())
    );
}

#[test]
fn parse_event_filter_keeps_any_text_and_refuses_an_empty_value() {
    assert_eq!(parse_event_filter("pane"), Ok("pane".to_string()));
    assert_eq!(parse_event_filter("TabMoved"), Ok("TabMoved".to_string()));
    assert_eq!(parse_event_filter(" "), Ok(" ".to_string()));
    assert_eq!(parse_event_filter("☕"), Ok("☕".to_string()));
    assert_eq!(
        parse_event_filter(""),
        Err("expected part of an event name, such as pane or TabMoved".to_string())
    );
}

#[test]
fn parse_event_age_reads_a_count_and_one_unit_character() {
    const EXPECTED_DURATION_ERROR: &str = "expected a length such as 30s, 15m, 24h or 7d";

    assert_eq!(parse_event_age("45s"), Ok(Duration::from_secs(45)));
    assert_eq!(parse_event_age("5m"), Ok(Duration::from_secs(300)));
    assert_eq!(parse_event_age("2h"), Ok(Duration::from_secs(7_200)));
    assert_eq!(parse_event_age("7d"), Ok(Duration::from_secs(604_800)));
    assert_eq!(parse_event_age("0s"), Ok(Duration::ZERO));
    assert_eq!(parse_event_age("007h"), Ok(Duration::from_secs(25_200)));

    for refused_text in ["", "5", "s", "5w", "5S", "5 s", "-5s", "never", "五s"] {
        assert_eq!(
            parse_event_age(refused_text),
            Err(EXPECTED_DURATION_ERROR.to_string()),
            "for {refused_text:?}"
        );
    }
    // u64::MAX days: the count parses, the multiply by 86400 does not.
    assert_eq!(
        parse_event_age("18446744073709551615d"),
        Err(EXPECTED_DURATION_ERROR.to_string())
    );
    // Seconds need no multiply, so the same count is taken.
    assert_eq!(
        parse_event_age("18446744073709551615s"),
        Ok(Duration::from_secs(u64::MAX))
    );
}

#[test]
fn resolve_tab_reference_id_yields_an_id_only_when_the_flag_carried_one() {
    let tab_id = TabId::from_uuid(build_fixed_test_uuid());
    assert_eq!(
        resolve_tab_reference_id(&Some(TabReference::TabId(tab_id))),
        Some(tab_id)
    );
    assert_eq!(
        resolve_tab_reference_id(&Some(TabReference::TabName("logs".to_string()))),
        None
    );
    assert_eq!(resolve_tab_reference_id(&None), None);
}

#[test]
fn build_spawn_spec_from_arguments_takes_the_first_token_as_the_program() {
    assert_eq!(
        build_spawn_spec_from_arguments(&["htop".to_string(), "-d".to_string(), "5".to_string()]),
        SpawnSpec {
            program: PathBuf::from("htop"),
            arguments: vec!["-d".to_string(), "5".to_string()],
            working_directory: None,
            environment_variables: BTreeMap::new(),
            shell_kind: ShellKind::Other("htop".to_string()),
        }
    );
    // The shell kind is the program path's file stem, lowercased.
    assert_eq!(
        build_spawn_spec_from_arguments(&["/bin/BASH".to_string()]).shell_kind,
        ShellKind::Bash
    );
    assert_eq!(
        build_spawn_spec_from_arguments(&["./scripts/deploy.sh".to_string()]).shell_kind,
        ShellKind::Other("deploy".to_string())
    );
}

#[test]
#[should_panic(expected = "index out of bounds")]
fn build_spawn_spec_from_arguments_panics_on_empty_arguments() {
    build_spawn_spec_from_arguments(&[]);
}

#[test]
fn a_session_reference_prints_as_the_user_named_it() {
    let uuid = build_fixed_test_uuid();
    assert_eq!(
        SessionReference::SessionId(SessionId::from_uuid(uuid)).to_string(),
        format!("session-{uuid}")
    );
    assert_eq!(
        SessionReference::SessionName("work".to_string()).to_string(),
        "work"
    );
    assert_eq!(SessionReference::SessionName(String::new()).to_string(), "");
}

// --- Action mapping ---

#[test]
fn action_subcommands_map_to_their_exact_commands() {
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let pane_flag = format!("pane-{}", build_fixed_test_uuid());
    let tab_id = TabId::from_uuid(build_fixed_test_uuid());
    let tab_flag = format!("tab-{}", build_fixed_test_uuid());
    let command_cases: Vec<(Vec<&str>, &str, Command)> = vec![
        (
            vec!["koshi", "new-pane", "--direction", "right"],
            "new-pane",
            Command::NewPane(NewPaneArgs {
                placement: NewPanePlacement::Split {
                    source_pane_id: None,
                    tab_id: None,
                    direction: Direction::Right,
                },
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            }),
        ),
        (
            vec!["koshi", "new-pane", "--stacked", "--pane", &pane_flag],
            "new-pane",
            Command::NewPane(NewPaneArgs {
                placement: NewPanePlacement::Stacked {
                    source_pane_id: Some(pane_id),
                    tab_id: None,
                },
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            }),
        ),
        (
            vec!["koshi", "close-pane", "--force"],
            "close-pane",
            Command::ClosePane(ClosePaneArgs {
                pane_id: None,
                should_force_close: true,
                should_kill_process_tree: false,
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
                pane_id: None,
                direction: Direction::Left,
                resize_amount_cells: -5,
            }),
        ),
        (
            vec!["koshi", "move-pane", "--direction", "up"],
            "move-pane",
            Command::MovePane(MovePaneArgs {
                pane_id: None,
                direction: Direction::Up,
            }),
        ),
        (
            vec![
                "koshi",
                "move-pane",
                "--direction",
                "up",
                "--pane",
                &pane_flag,
            ],
            "move-pane",
            Command::MovePane(MovePaneArgs {
                pane_id: Some(pane_id),
                direction: Direction::Up,
            }),
        ),
        (
            vec![
                "koshi",
                "place-pane",
                "--pane",
                &pane_flag,
                "--tab",
                &tab_flag,
                "--direction",
                "left",
            ],
            "place-pane",
            Command::PlacePane(PlacePaneArgs {
                source_pane_id: pane_id,
                placement_target: PanePlacementTarget::Split {
                    destination_tab_id: tab_id,
                    anchor: PanePlacementAnchor::Tab,
                    direction: Direction::Left,
                },
                expected_placement_revision: None,
            }),
        ),
        (
            vec![
                "koshi",
                "scroll-pane",
                "--lines",
                "-5",
                "--pane",
                &pane_flag,
            ],
            "scroll-pane",
            Command::ScrollPane(ScrollPaneArgs {
                pane_id: Some(pane_id),
                scroll_line_count: -5,
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
                working_directory: None,
                client_id: None,
            }),
        ),
        (
            vec!["koshi", "close-tab", "--tab", &tab_flag],
            "close-tab",
            Command::CloseTab(CloseTabArgs {
                tab_id: Some(tab_id),
                should_force_close: false,
                should_kill_process_tree: false,
            }),
        ),
        (
            vec!["koshi", "next-tab"],
            "next-tab",
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Next,
                client_id: None,
            }),
        ),
        (
            vec!["koshi", "previous-tab"],
            "previous-tab",
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Previous,
                client_id: None,
            }),
        ),
        (
            vec!["koshi", "move-tab", "--index", "3", "--tab", &tab_flag],
            "move-tab",
            Command::MoveTab(MoveTabArgs {
                tab_id: Some(tab_id),
                target_tab_index: 3,
            }),
        ),
        (
            vec!["koshi", "focus-tab", "--index", "0"],
            "focus-tab",
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Index(0),
                client_id: None,
            }),
        ),
        (
            vec!["koshi", "focus-tab", "--tab", &tab_flag],
            "focus-tab",
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Id(tab_id),
                client_id: None,
            }),
        ),
        (
            vec!["koshi", "focus-pane", "--pane", &pane_flag],
            "focus-pane",
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Pane(pane_id),
                client_id: None,
            }),
        ),
        (
            vec!["koshi", "lock"],
            "lock",
            Command::SetLockMode(LockModeArgs {
                is_locked: true,
                client_id: None,
            }),
        ),
        (
            vec!["koshi", "unlock"],
            "unlock",
            Command::SetLockMode(LockModeArgs {
                is_locked: false,
                client_id: None,
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
            Command::NewPane(NewPaneArgs {
                placement: NewPanePlacement::Stacked {
                    source_pane_id: None,
                    tab_id: None,
                },
                working_directory: None,
                spawn_spec: Some(SpawnSpec {
                    program: PathBuf::from("htop"),
                    arguments: vec!["-d".to_string(), "5".to_string()],
                    working_directory: None,
                    environment_variables: BTreeMap::new(),
                    shell_kind: ShellKind::Other("htop".to_string()),
                }),
                client_id: None,
            }),
        ),
    ];

    for (argument_values, action_name, expected_command) in command_cases {
        let (action_reference, mapped_command) = build_cli_action(&argument_values);
        assert_eq!(
            action_reference,
            ActionReference::from_core_action_name(action_name)
                .expect("test action names are valid"),
            "for {argument_values:?}"
        );
        assert_eq!(mapped_command, expected_command, "for {argument_values:?}");
    }
}

#[test]
fn every_mapped_action_matches_its_seeded_command_kind() {
    // Each argument list exercises one CLI action surface. Its mapping agrees with the
    // seed table on the action's existence and on the command kind it
    // dispatches.
    let seeds = build_core_action_seeds();
    let argument_value_lists: &[&[&str]] = &[
        &["koshi", "new-pane"],
        &["koshi", "close-pane"],
        &["koshi", "resize-pane", "--direction", "left"],
        &["koshi", "move-pane", "--direction", "right"],
        &["koshi", "scroll-pane", "--lines", "3"],
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

    for argument_values in argument_value_lists {
        let (action_reference, mapped_command) = build_cli_action(argument_values);
        let (_, action_metadata) = seeds
            .iter()
            .find(|(seeded_action_reference, _)| *seeded_action_reference == action_reference)
            .unwrap_or_else(|| panic!("action {action_reference} is not in the seed table"));
        let ActionHandlerReference::CoreCommand(expected_command_kind) = action_metadata.handler
        else {
            panic!("action {action_reference} is seeded with an unexpected handler");
        };
        // `Command::NewPane(NewPaneArgs { .. })` prints `NewPane(...)`, and
        // `CommandKind::NewPane` prints `NewPane`.
        let mapped_debug_text = format!("{mapped_command:?}");
        let mapped_variant_name = mapped_debug_text
            .split(['(', ' '])
            .next()
            .expect("split yields at least one piece");
        assert_eq!(
            mapped_variant_name,
            format!("{expected_command_kind:?}"),
            "for {argument_values:?}"
        );
    }
}

#[test]
fn non_action_subcommands_map_to_none() {
    let argument_value_lists: &[&[&str]] = &[
        &["koshi", "list-sessions"],
        &["koshi", "kill-session"],
        &["koshi", "attach"],
        &["koshi", "detach"],
        &["koshi", "doctor"],
        &["koshi", "config", "path"],
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
    for argument_values in argument_value_lists {
        let parsed_command = parse_cli_command(argument_values);
        assert!(
            !parsed_command.is_action_verb(),
            "{argument_values:?} must not be an action"
        );
        assert_eq!(
            parsed_command.build_action_command(&ResolvedTargets::default(), Direction::Right),
            None,
            "for {argument_values:?}"
        );
    }
}

#[test]
fn the_self_run_and_store_verbs_map_to_no_action() {
    let session_id_text = format!("session-{}", build_fixed_test_uuid());
    let argument_value_lists: Vec<Vec<&str>> = vec![
        vec!["koshi", "update"],
        vec!["koshi", "restart-servers"],
        vec!["koshi", "resume-support"],
        vec!["koshi", "serve-router"],
        vec!["koshi", "serve-session", &session_id_text, "amber-fox"],
        vec!["koshi", "serve-pty-supervisor", &session_id_text, "token"],
        vec!["koshi", "share", "list"],
        vec!["koshi", "remote", "list"],
    ];
    for argument_values in &argument_value_lists {
        let parsed_command = parse_cli_command(argument_values);
        assert!(
            !parsed_command.is_action_verb(),
            "{argument_values:?} must not be an action"
        );
        assert_eq!(
            parsed_command.build_action_command(&ResolvedTargets::default(), Direction::Right),
            None,
            "for {argument_values:?}"
        );
    }
}

// --- Routing targets ---

#[test]
fn target_session_names_the_session_of_every_verb_that_takes_one() {
    let argument_value_lists: Vec<Vec<&str>> = vec![
        vec!["koshi", "new-pane", "--session", "work"],
        vec!["koshi", "run", "--session", "work", "--", "htop"],
        vec!["koshi", "new-tab", "--session", "work"],
        vec!["koshi", "close-tab", "--session", "work"],
    ];
    for argument_values in &argument_value_lists {
        assert_eq!(
            parse_cli_command(argument_values).get_target_session_reference(),
            Some(&SessionReference::SessionName("work".to_string())),
            "for {argument_values:?}"
        );
    }
    // A listing's `--session` is a discovery scope, not a routing target.
    assert_eq!(
        parse_cli_command(&["koshi", "list-tabs", "--session", "work"])
            .get_target_session_reference(),
        None
    );
    assert_eq!(
        parse_cli_command(&["koshi", "kill-session", "work"]).get_target_session_reference(),
        None
    );
    assert_eq!(
        parse_cli_command(&["koshi", "new-pane"]).get_target_session_reference(),
        None
    );
}

#[test]
fn target_tab_names_the_tab_of_every_verb_that_takes_one() {
    let argument_value_lists: Vec<Vec<&str>> = vec![
        vec!["koshi", "new-pane", "--tab", "logs"],
        vec!["koshi", "run", "--tab", "logs", "--", "htop"],
        vec!["koshi", "close-tab", "--tab", "logs"],
        vec!["koshi", "move-tab", "--index", "0", "--tab", "logs"],
        vec!["koshi", "focus-tab", "--tab", "logs"],
        vec![
            "koshi",
            "place-pane",
            "--pane",
            "pane-0192f0c1-2345-7000-8000-000000000001",
            "--tab",
            "logs",
            "--direction",
            "left",
        ],
    ];
    for argument_values in &argument_value_lists {
        assert_eq!(
            parse_cli_command(argument_values).get_target_tab_reference(),
            Some(&TabReference::TabName("logs".to_string())),
            "for {argument_values:?}"
        );
    }
    // `debug dump-layout` takes a `--tab` that narrows the dump, not a route.
    assert_eq!(
        parse_cli_command(&["koshi", "debug", "dump-layout", "--tab", "logs"])
            .get_target_tab_reference(),
        None
    );
    assert_eq!(
        parse_cli_command(&["koshi", "new-pane"]).get_target_tab_reference(),
        None
    );
}

#[test]
fn target_pane_names_the_pane_of_every_verb_that_takes_one() {
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let pane_flag = format!("pane-{}", build_fixed_test_uuid());
    let with_pane: Vec<Vec<&str>> = vec![
        vec!["koshi", "new-pane", "--pane", &pane_flag],
        vec!["koshi", "run", "--pane", &pane_flag, "--", "htop"],
        vec!["koshi", "close-pane", "--pane", &pane_flag],
        vec![
            "koshi",
            "resize-pane",
            "--direction",
            "up",
            "--pane",
            &pane_flag,
        ],
        vec![
            "koshi",
            "move-pane",
            "--direction",
            "left",
            "--pane",
            &pane_flag,
        ],
        vec![
            "koshi",
            "place-pane",
            "--pane",
            &pane_flag,
            "--tab",
            "logs",
            "--direction",
            "left",
        ],
        vec!["koshi", "scroll-pane", "--lines", "3", "--pane", &pane_flag],
        vec!["koshi", "input", "--pane", &pane_flag, "ls"],
        vec!["koshi", "focus-pane", "--pane", &pane_flag],
    ];
    for argument_values in &with_pane {
        assert_eq!(
            parse_cli_command(argument_values).get_target_pane_id(),
            Some(pane_id),
            "for {argument_values:?}"
        );
    }

    let without_pane: Vec<Vec<&str>> = vec![
        vec!["koshi", "new-pane"],
        vec!["koshi", "close-pane"],
        vec!["koshi", "input", "ls"],
        vec!["koshi", "move-pane", "--direction", "left"],
        vec!["koshi", "new-tab"],
        vec!["koshi", "lock"],
        vec!["koshi", "list-panes"],
    ];
    for argument_values in &without_pane {
        assert_eq!(
            parse_cli_command(argument_values).get_target_pane_id(),
            None,
            "for {argument_values:?}"
        );
    }
}

#[test]
fn target_client_names_the_client_of_every_verb_that_takes_one() {
    let client_id = ClientId::from_uuid(build_fixed_test_uuid());
    let client_flag = format!("client-{}", build_fixed_test_uuid());
    let pane_flag = format!("pane-{}", build_fixed_test_uuid());
    // (arguments, whether the client rides on the command source)
    let argument_value_lists: Vec<(Vec<&str>, bool)> = vec![
        (vec!["koshi", "new-pane", "--client", &client_flag], false),
        (
            vec!["koshi", "run", "--client", &client_flag, "--", "htop"],
            false,
        ),
        (vec!["koshi", "new-tab", "--client", &client_flag], false),
        (vec!["koshi", "next-tab", "--client", &client_flag], false),
        (
            vec!["koshi", "previous-tab", "--client", &client_flag],
            false,
        ),
        (
            vec![
                "koshi",
                "focus-tab",
                "--index",
                "0",
                "--client",
                &client_flag,
            ],
            false,
        ),
        (
            vec![
                "koshi",
                "focus-pane",
                "--pane",
                &pane_flag,
                "--client",
                &client_flag,
            ],
            false,
        ),
        (vec!["koshi", "lock", "--client", &client_flag], false),
        (vec!["koshi", "unlock", "--client", &client_flag], false),
        (
            vec!["koshi", "toggle-lock", "--client", &client_flag],
            false,
        ),
        (
            vec!["koshi", "toggle-pane-fullscreen", "--client", &client_flag],
            true,
        ),
        (
            vec![
                "koshi",
                "place-pane",
                "--pane",
                &pane_flag,
                "--tab",
                "logs",
                "--direction",
                "left",
                "--client",
                &client_flag,
            ],
            true,
        ),
        (
            vec![
                "koshi",
                "scroll-pane",
                "--lines",
                "3",
                "--client",
                &client_flag,
            ],
            true,
        ),
        (
            vec![
                "koshi",
                "resize-pane",
                "--direction",
                "left",
                "--client",
                &client_flag,
            ],
            true,
        ),
    ];
    for (argument_values, is_client_on_source) in &argument_value_lists {
        let cli_command = parse_cli_command(argument_values);
        assert_eq!(
            cli_command.get_target_client_id(),
            Some(client_id),
            "for {argument_values:?}"
        );
        assert_eq!(
            cli_command.get_source_client_id(),
            is_client_on_source.then_some(client_id),
            "for {argument_values:?}"
        );
    }

    for argument_values in [
        vec!["koshi", "close-pane"],
        vec!["koshi", "move-tab", "--index", "0"],
        vec!["koshi", "toggle-pane-fullscreen"],
    ] {
        let cli_command = parse_cli_command(&argument_values);
        assert_eq!(
            cli_command.get_target_client_id(),
            None,
            "for {argument_values:?}"
        );
        assert_eq!(
            cli_command.get_source_client_id(),
            None,
            "for {argument_values:?}"
        );
    }
}

/// A `--tab` the routing layer resolved wins over the same flag given
/// directly as an id.
#[test]
fn a_resolved_tab_target_wins_over_a_tab_flag_given_as_an_id() {
    let flag_tab_id = TabId::from_uuid(build_fixed_test_uuid());
    let resolved_tab_id = TabId::new();
    assert_ne!(flag_tab_id, resolved_tab_id);
    let tab_flag = flag_tab_id.to_string();
    let resolved_targets = ResolvedTargets {
        session_id: None,
        tab_id: Some(resolved_tab_id),
    };

    let (_, mapped_command) = parse_cli_command(&["koshi", "close-tab", "--tab", &tab_flag])
        .build_action_command(&resolved_targets, Direction::Right)
        .expect("close-tab is an action");
    assert_eq!(
        mapped_command,
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(resolved_tab_id),
            should_force_close: false,
            should_kill_process_tree: false,
        })
    );

    let (_, mapped_command) = parse_cli_command(&["koshi", "new-pane", "--tab", &tab_flag])
        .build_action_command(&resolved_targets, Direction::Right)
        .expect("new-pane is an action");
    assert_eq!(
        mapped_command,
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: None,
                tab_id: Some(resolved_tab_id),
                direction: Direction::Right,
            },
            working_directory: None,
            spawn_spec: None,
            client_id: None,
        })
    );
}

// --- Discovery scope ---

#[test]
fn every_inspect_form_is_a_discovery_query() {
    let pane_id_text = format!("pane-{}", build_fixed_test_uuid());
    let client_id_text = format!("client-{}", build_fixed_test_uuid());
    assert!(parse_cli_command(&["koshi", "inspect", "tab", "logs"]).is_discovery_query());
    assert!(parse_cli_command(&["koshi", "inspect", "pane", &pane_id_text]).is_discovery_query());
    assert!(
        parse_cli_command(&["koshi", "inspect", "client", &client_id_text]).is_discovery_query()
    );

    for argument_values in [
        vec!["koshi", "new-pane"],
        vec!["koshi", "close-pane"],
        vec!["koshi", "lock"],
        vec!["koshi", "attach"],
        vec!["koshi", "doctor"],
        vec!["koshi", "keys", "list"],
        vec!["koshi", "debug", "dump-state"],
    ] {
        assert!(
            !parse_cli_command(&argument_values).is_discovery_query(),
            "for {argument_values:?}"
        );
    }
}

#[test]
fn only_a_listing_flag_or_an_inspected_session_names_a_discovery_session() {
    let pane_id_text = format!("pane-{}", build_fixed_test_uuid());
    let client_id_text = format!("client-{}", build_fixed_test_uuid());
    for argument_values in [
        vec!["koshi", "list-tabs", "--session", "main"],
        vec!["koshi", "list-panes", "--session", "main"],
        vec!["koshi", "list-clients", "--session", "main"],
        vec!["koshi", "inspect", "session", "main"],
    ] {
        assert_eq!(
            parse_cli_command(&argument_values).get_discovery_session_reference(),
            Some(&SessionReference::SessionName("main".to_string())),
            "for {argument_values:?}"
        );
    }

    for argument_values in [
        vec!["koshi", "list-sessions"],
        vec!["koshi", "list-panes"],
        vec!["koshi", "list-clients"],
        vec!["koshi", "inspect", "tab", "logs"],
        vec!["koshi", "inspect", "pane", &pane_id_text],
        vec!["koshi", "inspect", "client", &client_id_text],
        vec!["koshi", "new-pane", "--session", "main"],
    ] {
        assert_eq!(
            parse_cli_command(&argument_values).get_discovery_session_reference(),
            None,
            "for {argument_values:?}"
        );
    }
}

// --- Adversarial: duplicate flags, boundaries, and unicode ---

#[test]
fn a_repeated_single_valued_flag_is_a_usage_error_not_a_last_wins() {
    // A single-valued flag given twice is a usage error, for a root flag
    // (`--profile`) and a subcommand flag (`--format`) alike.
    let profile_twice = parse_cli_error(&["koshi", "--profile", "first", "--profile", "second"]);
    assert_eq!(profile_twice.kind(), ErrorKind::ArgumentConflict);
    assert_eq!(profile_twice.exit_code(), 2);

    let format_twice = parse_cli_error(&[
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
    // `attach` keeps its positional as typed, with no value parser on it.
    assert_eq!(
        parse_cli_arguments(&["koshi", "attach", ""]).command,
        Some(CliCommand::Attach {
            session_argument: Some(String::new()),
            save_as: None,
        })
    );
}

#[test]
fn attach_accepts_a_unicode_session_id() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "attach", "café-上海"]).command,
        Some(CliCommand::Attach {
            session_argument: Some("café-上海".to_string()),
            save_as: None,
        })
    );
}

/// A session or tab argument runs a value parser that refuses an empty value,
/// so clap reports it before the verb runs.
#[test]
fn an_empty_session_or_tab_argument_is_a_usage_error() {
    for argument_values in [
        vec!["koshi", "kill-session", ""],
        vec!["koshi", "list-tabs", "--session", ""],
        vec!["koshi", "new-tab", "--session", ""],
        vec!["koshi", "inspect", "session", ""],
        vec!["koshi", "close-tab", "--tab", ""],
        vec!["koshi", "inspect", "tab", ""],
        vec!["koshi", "debug", "dump-layout", "--tab", ""],
    ] {
        let cli_error = parse_cli_error(&argument_values);
        assert_eq!(
            cli_error.kind(),
            ErrorKind::ValueValidation,
            "for {argument_values:?}"
        );
        assert_eq!(cli_error.exit_code(), 2, "for {argument_values:?}");
    }
}

/// The text reaches the pane as its own UTF-8 bytes, empty text included.
#[test]
fn input_sends_the_text_bytes_as_typed() {
    let (_, mapped_command) = build_cli_action(&["koshi", "input", ""]);
    assert_eq!(
        mapped_command,
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: None,
            pane_input_bytes: b"\r".to_vec(),
        })
    );

    let (_, mapped_command) = build_cli_action(&["koshi", "input", "--no-enter", ""]);
    assert_eq!(
        mapped_command,
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: None,
            pane_input_bytes: Vec::new(),
        })
    );

    let (_, mapped_command) = build_cli_action(&["koshi", "input", "echo ☕"]);
    assert_eq!(
        mapped_command,
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: None,
            pane_input_bytes: "echo ☕\r".as_bytes().to_vec(),
        })
    );

    // A newline inside the text is kept, and Enter still follows it.
    let (_, mapped_command) = build_cli_action(&["koshi", "input", "a\nb"]);
    assert_eq!(
        mapped_command,
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: None,
            pane_input_bytes: b"a\nb\r".to_vec(),
        })
    );
}

#[test]
fn focus_tab_index_rejects_a_negative_number() {
    let cli_error = parse_cli_error(&["koshi", "focus-tab", "--index", "-1"]);
    assert_eq!(cli_error.kind(), ErrorKind::UnknownArgument);
}

#[test]
fn focus_tab_index_rejects_an_overflowing_number() {
    // One digit past `usize::MAX` (18446744073709551615 on a 64-bit target).
    let cli_error = parse_cli_error(&["koshi", "focus-tab", "--index", "18446744073709551616"]);
    assert_eq!(cli_error.kind(), ErrorKind::ValueValidation);
}

#[test]
fn resize_pane_size_accepts_the_i16_boundaries() {
    assert_eq!(
        parse_cli_command(&[
            "koshi",
            "resize-pane",
            "--direction",
            "up",
            "--size",
            "32767"
        ]),
        CliCommand::ResizePane {
            direction: DirectionArgument::Up,
            resize_amount_cells: i16::MAX,
            pane_id: None,
            client_id: None,
        }
    );
    assert_eq!(
        parse_cli_command(&[
            "koshi",
            "resize-pane",
            "--direction",
            "up",
            "--size",
            "-32768"
        ]),
        CliCommand::ResizePane {
            direction: DirectionArgument::Up,
            resize_amount_cells: i16::MIN,
            pane_id: None,
            client_id: None,
        }
    );
}

#[test]
fn resize_pane_size_rejects_i16_overflow() {
    let cli_error = parse_cli_error(&[
        "koshi",
        "resize-pane",
        "--direction",
        "up",
        "--size",
        "32768",
    ]);
    assert_eq!(cli_error.kind(), ErrorKind::ValueValidation);
}

#[test]
fn output_format_name_is_case_sensitive() {
    let cli_error = parse_cli_error(&["koshi", "list-sessions", "--format", "Table"]);
    assert_eq!(cli_error.kind(), ErrorKind::InvalidValue);
}

#[test]
fn an_id_with_only_the_prefix_and_a_dash_is_rejected() {
    let cli_error = parse_cli_error(&["koshi", "close-pane", "--pane", "pane-"]);
    assert_eq!(cli_error.kind(), ErrorKind::ValueValidation);
}

#[test]
fn a_prefix_collision_without_a_separating_dash_is_rejected() {
    // "panes-<uuid>" strips as far as "pane" (a true prefix of "panes"),
    // leaving "s-<uuid>" — which does not start with '-', so the dash-strip
    // fails and the whole original string is tried as a bare UUID, which it
    // is not.
    let pane_selector = format!("panes-{}", build_fixed_test_uuid());
    let cli_error = parse_cli_error(&["koshi", "close-pane", "--pane", &pane_selector]);
    assert_eq!(cli_error.kind(), ErrorKind::ValueValidation);
}

#[test]
fn a_session_value_that_is_not_an_id_parses_as_a_name() {
    // `--session` takes a name or an id. A value that does not read as an id
    // — here a "sessions-" prefix — is kept whole as a name, and routing
    // refuses it subsequently when no session bears it.
    let session_selector = format!("sessions-{}", build_fixed_test_uuid());
    assert_eq!(
        parse_cli_command(&["koshi", "new-tab", "--session", &session_selector]),
        CliCommand::NewTab {
            session_reference: Some(SessionReference::SessionName(session_selector)),
            client_id: None,
        }
    );
}

#[test]
fn id_parse_error_message_names_the_expected_forms() {
    assert_eq!(
        parse_pane_id("not-a-uuid"),
        Err("expected `pane-<uuid>` or a bare UUID".to_string())
    );
    assert_eq!(
        parse_cli_error(&["koshi", "close-pane", "--pane", "not-a-uuid"]).to_string(),
        "error: invalid value 'not-a-uuid' for '--pane <PANE_ID>': \
         expected `pane-<uuid>` or a bare UUID\n\n\
         For more information, try '--help'.\n"
    );
}

#[test]
fn a_bare_uppercase_uuid_parses() {
    // The UUID's own hex digits are case-insensitive even though the
    // `<prefix>-` stripping is a case-sensitive byte match.
    let uppercase_uuid_text = build_fixed_test_uuid().to_string().to_uppercase();
    assert_eq!(
        parse_cli_command(&["koshi", "close-pane", "--pane", &uppercase_uuid_text]),
        CliCommand::ClosePane {
            pane_id: Some(PaneId::from_uuid(build_fixed_test_uuid())),
            should_force_close: false,
        }
    );
}

#[test]
fn run_accepts_an_empty_program_token() {
    assert_eq!(
        parse_cli_command(&["koshi", "run", "--", ""]),
        CliCommand::Run {
            new_pane_flags: NewPaneFlags {
                direction: None,
                should_stack: false,
                pane_id: None,
                session_reference: None,
                tab_reference: None,
                client_id: None,
            },
            command_arguments: vec![String::new()],
        }
    );
    let (_, mapped_command) = build_cli_action(&["koshi", "run", "--", ""]);
    assert_eq!(
        mapped_command,
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Right,
            },
            working_directory: None,
            spawn_spec: Some(SpawnSpec {
                program: PathBuf::new(),
                arguments: vec![],
                working_directory: None,
                environment_variables: BTreeMap::new(),
                shell_kind: ShellKind::Other(String::new()),
            }),
            client_id: None,
        })
    );
}

#[test]
fn run_program_name_is_preserved_verbatim_for_non_ascii() {
    let (_, mapped_command) = build_cli_action(&["koshi", "run", "--", "☕"]);
    let Command::NewPane(NewPaneArgs {
        spawn_spec: Some(spawn_spec),
        ..
    }) = mapped_command
    else {
        panic!("expected NewPane carrying a program");
    };
    assert_eq!(spawn_spec.program, PathBuf::from("☕"));
    assert_eq!(spawn_spec.shell_kind, ShellKind::Other("☕".to_string()));
}

// --- Session and tab arguments: id or name, verb by verb ---

/// The [`SessionReference`] the session-taking verb in `argument_values` parsed.
fn parse_session_reference_from_arguments(argument_values: &[&str]) -> SessionReference {
    match parse_cli_command(argument_values) {
        CliCommand::KillSession { session_reference }
        | CliCommand::ListTabs {
            session_reference, ..
        }
        | CliCommand::ListPanes {
            session_reference, ..
        }
        | CliCommand::ListClients {
            session_reference, ..
        } => session_reference.expect("the arguments name a session"),
        CliCommand::Inspect {
            inspect_target:
                InspectTarget::Session {
                    session_reference, ..
                },
        } => session_reference,
        unexpected_command => panic!("the arguments name no session: {unexpected_command:?}"),
    }
}

/// The [`TabReference`] the tab-taking verb in `argument_values` parsed.
fn parse_tab_reference_from_arguments(argument_values: &[&str]) -> TabReference {
    match parse_cli_command(argument_values) {
        CliCommand::MoveTab { tab_reference, .. } | CliCommand::FocusTab { tab_reference, .. } => {
            tab_reference.expect("the arguments name a tab")
        }
        CliCommand::Inspect {
            inspect_target: InspectTarget::Tab { tab_reference, .. },
        } => tab_reference,
        unexpected_command => panic!("the arguments name no tab: {unexpected_command:?}"),
    }
}

/// Every verb taking a session runs the one id-else-name gate:
/// `session-<uuid>` is an id, `work` is a name.
#[test]
fn every_session_argument_parses_an_id_or_a_name() {
    let session_argument = format!("session-{}", build_fixed_test_uuid());
    let verb_prefixes: &[&[&str]] = &[
        &["koshi", "kill-session"],
        &["koshi", "list-tabs", "--session"],
        &["koshi", "list-panes", "--session"],
        &["koshi", "list-clients", "--session"],
        &["koshi", "inspect", "session"],
    ];
    for verb_prefix in verb_prefixes {
        let mut arguments_with_id = verb_prefix.to_vec();
        arguments_with_id.push(&session_argument);
        assert_eq!(
            parse_session_reference_from_arguments(&arguments_with_id),
            SessionReference::SessionId(SessionId::from_uuid(build_fixed_test_uuid())),
            "for {arguments_with_id:?}"
        );

        let mut arguments_with_name = verb_prefix.to_vec();
        arguments_with_name.push("work");
        assert_eq!(
            parse_session_reference_from_arguments(&arguments_with_name),
            SessionReference::SessionName("work".to_string()),
            "for {arguments_with_name:?}"
        );
    }
}

/// Every verb taking a tab runs the one id-else-name gate: `tab-<uuid>` is an
/// id, `logs` is a name.
#[test]
fn every_tab_argument_parses_an_id_or_a_name() {
    let tab_argument = format!("tab-{}", build_fixed_test_uuid());
    let verb_prefixes: &[&[&str]] = &[
        &["koshi", "move-tab", "--index", "2", "--tab"],
        &["koshi", "focus-tab", "--tab"],
        &["koshi", "inspect", "tab"],
    ];
    for verb_prefix in verb_prefixes {
        let mut arguments_with_id = verb_prefix.to_vec();
        arguments_with_id.push(&tab_argument);
        assert_eq!(
            parse_tab_reference_from_arguments(&arguments_with_id),
            TabReference::TabId(TabId::from_uuid(build_fixed_test_uuid())),
            "for {arguments_with_id:?}"
        );

        let mut arguments_with_name = verb_prefix.to_vec();
        arguments_with_name.push("logs");
        assert_eq!(
            parse_tab_reference_from_arguments(&arguments_with_name),
            TabReference::TabName("logs".to_string()),
            "for {arguments_with_name:?}"
        );
    }
}

/// A `--tab` id rides into the mapped command with no resolved targets; a
/// `--tab` name rides in as the id the routing layer resolved it to.
#[test]
fn move_tab_and_focus_tab_carry_their_tab_id_into_the_command() {
    let tab_id = TabId::from_uuid(build_fixed_test_uuid());
    let tab_flag = format!("tab-{}", build_fixed_test_uuid());

    let (_, mapped_command) =
        build_cli_action(&["koshi", "move-tab", "--index", "2", "--tab", &tab_flag]);
    assert_eq!(
        mapped_command,
        Command::MoveTab(MoveTabArgs {
            tab_id: Some(tab_id),
            target_tab_index: 2,
        })
    );
    let (_, mapped_command) = build_cli_action(&["koshi", "focus-tab", "--tab", &tab_flag]);
    assert_eq!(
        mapped_command,
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(tab_id),
            client_id: None,
        })
    );

    let resolved_targets = ResolvedTargets {
        session_id: None,
        tab_id: Some(tab_id),
    };
    assert_eq!(
        parse_cli_command(&["koshi", "move-tab", "--index", "2", "--tab", "logs"])
            .build_action_command(&resolved_targets, Direction::Right),
        Some((
            ActionReference::from_core_action_name("move-tab").expect("valid"),
            Command::MoveTab(MoveTabArgs {
                tab_id: Some(tab_id),
                target_tab_index: 2,
            })
        ))
    );
    assert_eq!(
        parse_cli_command(&["koshi", "focus-tab", "--tab", "logs"])
            .build_action_command(&resolved_targets, Direction::Right),
        Some((
            ActionReference::from_core_action_name("focus-tab").expect("valid"),
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Id(tab_id),
                client_id: None,
            })
        ))
    );
}

#[test]
fn place_pane_with_a_named_tab_is_classified_before_routing_resolves_the_tab() {
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let pane_flag = format!("pane-{}", build_fixed_test_uuid());
    let destination_tab_id = TabId::from_uuid(build_fixed_test_uuid());
    let parsed_command = parse_cli_command(&[
        "koshi",
        "place-pane",
        "--pane",
        &pane_flag,
        "--tab",
        "monitoring",
        "--direction",
        "left",
    ]);

    assert!(parsed_command.is_action_verb());

    let (_, mapped_command) = parsed_command
        .build_action_command(
            &ResolvedTargets {
                session_id: None,
                tab_id: Some(destination_tab_id),
            },
            Direction::Right,
        )
        .expect("a routed place-pane command maps to a core command");
    assert_eq!(
        mapped_command,
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: pane_id,
            placement_target: PanePlacementTarget::Split {
                destination_tab_id,
                anchor: PanePlacementAnchor::Tab,
                direction: Direction::Left,
            },
            expected_placement_revision: None,
        })
    );
}

#[test]
fn the_version_verbs_parse() {
    assert_eq!(
        parse_cli_arguments(&["koshi", "version"]).command,
        Some(CliCommand::Version {
            output_format: OutputFormat::Table,
        })
    );
    assert_eq!(
        parse_cli_arguments(&["koshi", "version", "--format", "json"]).command,
        Some(CliCommand::Version {
            output_format: OutputFormat::Json,
        })
    );
    assert_eq!(
        parse_cli_arguments(&["koshi", "server-version"]).command,
        Some(CliCommand::ServerVersion {
            session_reference: None,
            output_format: OutputFormat::Table,
        })
    );
    assert_eq!(
        parse_cli_arguments(&["koshi", "server-version", "--format", "json"]).command,
        Some(CliCommand::ServerVersion {
            session_reference: None,
            output_format: OutputFormat::Json,
        })
    );
}

#[test]
fn server_version_takes_a_session_by_name_or_by_id() {
    let session_id = SessionId::new();
    assert_eq!(
        parse_cli_arguments(&["koshi", "server-version", "--session", "work"]).command,
        Some(CliCommand::ServerVersion {
            session_reference: Some(SessionReference::SessionName("work".to_string())),
            output_format: OutputFormat::Table,
        })
    );
    assert_eq!(
        parse_cli_arguments(&[
            "koshi",
            "server-version",
            "--session",
            &session_id.to_string()
        ])
        .command,
        Some(CliCommand::ServerVersion {
            session_reference: Some(SessionReference::SessionId(session_id)),
            output_format: OutputFormat::Table,
        })
    );
}

#[test]
fn server_version_rejects_an_unknown_format() {
    let cli_error = parse_cli_error(&["koshi", "server-version", "--format", "yaml"]);
    assert_eq!(cli_error.kind(), ErrorKind::InvalidValue);
}

#[test]
fn neither_version_verb_is_an_action_the_socket_serves() {
    assert_eq!(
        parse_cli_command(&["koshi", "version"])
            .build_action_command(&ResolvedTargets::default(), Direction::Right),
        None
    );
    assert_eq!(
        parse_cli_command(&["koshi", "server-version"])
            .build_action_command(&ResolvedTargets::default(), Direction::Right),
        None
    );
}

/// `koshi --remote work` names another machine and is not the interactive
/// launch.
#[test]
fn a_bare_invocation_naming_a_server_is_not_the_interactive_launch() {
    let cli = parse_cli_arguments(&["koshi", "--remote", "work"]);

    assert_eq!(
        cli,
        Cli {
            is_headless: false,
            should_allow_other_users: false,
            profile_name: None,
            remote_server_reference: Some("work".to_string()),
            command: None,
        }
    );
    assert!(!cli.is_interactive_launch());
}

/// The discovery split: every `list-*` verb and every `inspect` form is a
/// discovery query, and an action verb is not.
#[test]
fn discovery_queries_are_the_listings_and_inspects() {
    assert!(parse_cli_command(&["koshi", "list-sessions"]).is_discovery_query());
    assert!(parse_cli_command(&["koshi", "list-tabs"]).is_discovery_query());
    assert!(parse_cli_command(&["koshi", "list-panes"]).is_discovery_query());
    assert!(parse_cli_command(&["koshi", "list-clients"]).is_discovery_query());
    assert!(parse_cli_command(&["koshi", "inspect", "session", "main"]).is_discovery_query());
    assert!(!parse_cli_command(&["koshi", "new-tab"]).is_discovery_query());
}

/// A discovery query names its one session — a listing's `--session` flag or
/// the session an `inspect session` targets — and spans all sessions
/// otherwise.
#[test]
fn a_discovery_query_names_its_session_scope() {
    assert_eq!(
        parse_cli_command(&["koshi", "list-tabs", "--session", "main"])
            .get_discovery_session_reference(),
        Some(&SessionReference::SessionName("main".to_string()))
    );
    assert_eq!(
        parse_cli_command(&["koshi", "inspect", "session", "main"])
            .get_discovery_session_reference(),
        Some(&SessionReference::SessionName("main".to_string()))
    );
    assert_eq!(
        parse_cli_command(&["koshi", "list-tabs"]).get_discovery_session_reference(),
        None
    );
    assert_eq!(
        parse_cli_command(&["koshi", "list-sessions"]).get_discovery_session_reference(),
        None
    );
}

/// Every action name `build_action_command` builds is a registered core action, and the
/// 19 action verbs name 19 different actions.
#[test]
fn every_cli_action_name_is_a_registered_core_action() {
    use std::collections::BTreeSet;

    let registered_core_action_names: BTreeSet<String> = build_core_action_seeds()
        .iter()
        .map(|(action_reference, _)| action_reference.to_string())
        .collect();

    // One argument list per CLI verb that maps to an action.
    let pane_id_text = PaneId::new().to_string();
    let action_verbs: Vec<Vec<&str>> = vec![
        vec!["koshi", "new-pane"],
        vec!["koshi", "close-pane"],
        vec!["koshi", "resize-pane", "--direction", "left"],
        vec!["koshi", "move-pane", "--direction", "left"],
        vec![
            "koshi",
            "place-pane",
            "--pane",
            &pane_id_text,
            "--tab",
            "tab-0192f0c1-2345-7000-8000-000000000001",
            "--direction",
            "left",
        ],
        vec!["koshi", "scroll-pane", "--lines", "3"],
        vec!["koshi", "toggle-pane-fullscreen"],
        vec!["koshi", "input", "echo hi"],
        vec!["koshi", "new-tab"],
        vec!["koshi", "close-tab"],
        vec!["koshi", "focus-tab", "--index", "1"],
        vec!["koshi", "move-tab", "--index", "1"],
        vec!["koshi", "next-tab"],
        vec!["koshi", "previous-tab"],
        vec!["koshi", "focus-pane", "--pane", &pane_id_text],
        vec!["koshi", "lock"],
        vec!["koshi", "unlock"],
        vec!["koshi", "toggle-lock"],
        vec!["koshi", "run", "--", "htop"],
    ];

    let mut cli_action_names = BTreeSet::new();
    for argument_values in &action_verbs {
        let parsed_command = parse_cli_command(argument_values);
        assert!(
            parsed_command.is_action_verb(),
            "{argument_values:?} must be an action"
        );
        let (action_reference, _) = parsed_command
            .build_action_command(&ResolvedTargets::default(), Direction::Right)
            .unwrap_or_else(|| panic!("{argument_values:?} maps to an action"));
        assert!(
            registered_core_action_names.contains(&action_reference.to_string()),
            "{argument_values:?} names {action_reference}, which build_core_action_seeds does not register"
        );
        cli_action_names.insert(action_reference.to_string());
    }
    let expected_registered_action_names: BTreeSet<String> = [
        "core:close-pane",
        "core:close-tab",
        "core:focus-pane",
        "core:focus-tab",
        "core:lock",
        "core:move-pane",
        "core:move-tab",
        "core:new-pane",
        "core:new-tab",
        "core:next-tab",
        "core:place-pane",
        "core:previous-tab",
        "core:resize-pane",
        "core:run",
        "core:scroll-pane",
        "core:toggle-lock",
        "core:toggle-pane-fullscreen",
        "core:unlock",
        "core:write-to-pane",
    ]
    .map(String::from)
    .into_iter()
    .collect();
    assert_eq!(cli_action_names, expected_registered_action_names);
}
