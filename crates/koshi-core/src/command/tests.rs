//! Tests for command serialization, variant canonicality, and validation.
//!
//! Covers roundtripping commands through JSON, verifying variant names and
//! discriminants are stable, and ensuring command envelopes validate client IDs.

use super::*;
use crate::event::{Event, QuitCause, RejectReason};
use crate::ids::{ClientId, CommandId, PaneId, PluginId, SessionId};
use serde_json::json;
use std::time::{Duration, UNIX_EPOCH};

/// A `new-pane` request with nothing chosen: the focused pane splits rightward.
fn build_new_pane_args() -> NewPaneArgs {
    NewPaneArgs {
        source_pane_id: None,
        tab_id: None,
        direction: Direction::Right,
        should_stack: false,
        working_directory: None,
        spawn_spec: None,
        client_id: None,
    }
}

/// Roundtrip a value through JSON and assert it survives unchanged.
fn assert_json_roundtrip<Roundtrippable>(roundtrippable_value: &Roundtrippable)
where
    Roundtrippable: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let serialized_json = serde_json::to_string(roundtrippable_value).expect("serialize");
    let decoded_roundtrippable_value: Roundtrippable =
        serde_json::from_str(&serialized_json).expect("deserialize");
    assert_eq!(*roundtrippable_value, decoded_roundtrippable_value);
}

#[test]
fn unit_commands_roundtrip() {
    assert_json_roundtrip(&Command::ToggleLockMode(ToggleLockModeArgs {
        client_id: Some(ClientId::new()),
    }));
    assert_json_roundtrip(&Command::TogglePaneFullscreen);
    assert_json_roundtrip(&Command::Quit);
}

#[test]
fn pane_commands_roundtrip() {
    assert_json_roundtrip(&Command::NewPane(NewPaneArgs {
        direction: Direction::Left,
        client_id: Some(ClientId::new()),
        ..build_new_pane_args()
    }));
    assert_json_roundtrip(&Command::ClosePane(ClosePaneArgs {
        pane_id: Some(PaneId::new()),
        should_force_close: true,
        should_kill_process_tree: true,
    }));
    assert_json_roundtrip(&Command::ResizePane(ResizePaneArgs {
        pane_id: None,
        direction: Direction::Up,
        resize_amount_cells: 4,
    }));
    assert_json_roundtrip(&Command::ResizePane(ResizePaneArgs {
        pane_id: Some(PaneId::new()),
        direction: Direction::Left,
        resize_amount_cells: -3,
    }));
    assert_json_roundtrip(&Command::MovePane(MovePaneArgs {
        pane_id: None,
        direction: Direction::Right,
    }));
    assert_json_roundtrip(&Command::SwapPanes(SwapPanesArgs {
        source_pane_id: Some(PaneId::new()),
        target_pane_id: PaneId::new(),
    }));
    assert_json_roundtrip(&Command::ScrollPane(ScrollPaneArgs {
        pane_id: Some(PaneId::new()),
        scroll_line_count: -7,
    }));
    assert_eq!(
        serde_json::to_value(Command::ScrollPane(ScrollPaneArgs {
            pane_id: None,
            scroll_line_count: -7,
        }))
        .expect("serialize scroll command"),
        json!({
            "ScrollPane": {
                "pane_id": null,
                "scroll_line_count": -7,
            }
        })
    );
    assert_json_roundtrip(&Command::RunCommandPane(RunCommandPaneArgs {
        spawn_spec: SpawnSpec {
            program: std::path::PathBuf::from("htop"),
            arguments: vec!["-d".to_string()],
            working_directory: None,
            environment_variables: std::collections::BTreeMap::new(),
            shell_kind: crate::process::ShellKind::Other("htop".to_string()),
        },
        working_directory: None,
        source_pane_id: Some(PaneId::new()),
        tab_id: Some(TabId::new()),
        direction: Direction::Down,
        should_stack: false,
        client_id: Some(ClientId::new()),
    }));
    assert_json_roundtrip(&Command::FocusPane(FocusPaneArgs {
        focus_target: FocusTarget::Pane(PaneId::new()),
        client_id: None,
    }));
    assert_json_roundtrip(&Command::FocusPane(FocusPaneArgs {
        focus_target: FocusTarget::Pane(PaneId::new()),
        client_id: Some(ClientId::new()),
    }));
    assert_json_roundtrip(&Command::FocusPane(FocusPaneArgs {
        focus_target: FocusTarget::Direction(Direction::Left),
        client_id: None,
    }));
}

#[test]
fn tab_and_session_commands_roundtrip() {
    assert_json_roundtrip(&Command::FocusTab(FocusTabArgs {
        focus_target: TabTarget::Next,
        client_id: None,
    }));
    assert_json_roundtrip(&Command::FocusTab(FocusTabArgs {
        focus_target: TabTarget::Index(2),
        client_id: None,
    }));
    assert_json_roundtrip(&Command::MoveTab(MoveTabArgs {
        tab_id: None,
        target_tab_index: 0,
    }));
}

#[test]
fn write_to_pane_roundtrips() {
    assert_json_roundtrip(&Command::WriteToPane(WriteToPaneArgs {
        pane_id: None,
        input_bytes: b"ls -la\n".to_vec(),
    }));
}

#[test]
fn visual_commands_roundtrip() {
    assert_json_roundtrip(&Command::Visual(VisualCommand::SetSelection(
        SetSelectionArgs {
            pane_id: PaneId::new(),
            selection: Selection {
                selection_kind: SelectionKind::Block,
                anchor: GridPosition {
                    row_index: 10,
                    column_index: 0,
                },
                cursor: GridPosition {
                    row_index: 12,
                    column_index: 40,
                },
            },
        },
    )));
    assert_json_roundtrip(&Command::Visual(VisualCommand::ClearSelection(
        ClearSelectionArgs {
            pane_id: PaneId::new(),
        },
    )));
    assert_json_roundtrip(&Command::Visual(VisualCommand::Copy(CopyArgs {
        pane_id: PaneId::new(),
        should_trim_trailing_whitespace: true,
        clipboard_target: CopyTarget::Osc52,
    })));
}

#[test]
fn plugin_commands_roundtrip() {
    assert_json_roundtrip(&Command::Plugin(PluginCommand::Install(
        InstallPluginArgs {
            plugin_source: "https://example.test/p.wasm".to_string(),
        },
    )));
    assert_json_roundtrip(&Command::Plugin(PluginCommand::Reload(ReloadPluginArgs {
        plugin_id: PluginId::new(),
    })));
}

/// The variant name from a value's Debug repr: everything before the first
/// `(`, `{`, or space, or the whole string for a unit variant.
fn get_variant_name<T: std::fmt::Debug>(debug_value: &T) -> String {
    let debug_text = format!("{debug_value:?}");
    let variant_end = debug_text.find(['(', '{', ' ']).unwrap_or(debug_text.len());
    debug_text[..variant_end].to_string()
}

/// One instance per top-level variant, paired with its canonical name.
#[test]
fn command_variant_names_are_canonical() {
    let command_cases: Vec<(Command, &str)> = vec![
        (Command::NewPane(build_new_pane_args()), "NewPane"),
        (Command::ClosePane(ClosePaneArgs::default()), "ClosePane"),
        (
            Command::ResizePane(ResizePaneArgs {
                pane_id: None,
                direction: Direction::Up,
                resize_amount_cells: 1,
            }),
            "ResizePane",
        ),
        (
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Pane(PaneId::new()),
                client_id: None,
            }),
            "FocusPane",
        ),
        (Command::NewTab(NewTabArgs::default()), "NewTab"),
        (Command::CloseTab(CloseTabArgs::default()), "CloseTab"),
        (
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Next,
                client_id: None,
            }),
            "FocusTab",
        ),
        (
            Command::WriteToPane(WriteToPaneArgs::default()),
            "WriteToPane",
        ),
        (
            Command::ToggleLockMode(ToggleLockModeArgs::default()),
            "ToggleLockMode",
        ),
        (
            Command::SetLockMode(LockModeArgs {
                is_locked: true,
                client_id: None,
            }),
            "SetLockMode",
        ),
        (
            Command::RunCommandPane(RunCommandPaneArgs {
                spawn_spec: SpawnSpec {
                    program: std::path::PathBuf::from("ls"),
                    arguments: vec![],
                    working_directory: None,
                    environment_variables: std::collections::BTreeMap::new(),
                    shell_kind: crate::process::ShellKind::Other("x".to_string()),
                },
                working_directory: None,
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Right,
                should_stack: false,
                client_id: None,
            }),
            "RunCommandPane",
        ),
        (
            Command::Visual(VisualCommand::ClearSelection(ClearSelectionArgs {
                pane_id: PaneId::new(),
            })),
            "Visual",
        ),
        (
            Command::Plugin(PluginCommand::Reload(ReloadPluginArgs {
                plugin_id: PluginId::new(),
            })),
            "Plugin",
        ),
        (Command::TogglePaneFullscreen, "TogglePaneFullscreen"),
        (
            Command::MoveTab(MoveTabArgs {
                tab_id: None,
                target_tab_index: 0,
            }),
            "MoveTab",
        ),
        (
            Command::MovePane(MovePaneArgs {
                pane_id: None,
                direction: Direction::Left,
            }),
            "MovePane",
        ),
        (
            Command::SwapPanes(SwapPanesArgs {
                source_pane_id: None,
                target_pane_id: PaneId::new(),
            }),
            "SwapPanes",
        ),
        (
            Command::ScrollPane(ScrollPaneArgs {
                pane_id: None,
                scroll_line_count: 1,
            }),
            "ScrollPane",
        ),
        (Command::Quit, "Quit"),
        (Command::ToggleMouseSelect, "ToggleMouseSelect"),
        (Command::Detach(DetachArgs::default()), "Detach"),
        (Command::DetachAll, "DetachAll"),
        (
            Command::SwitchSession(SwitchSessionArgs {
                client_id: None,
                session_id: SessionId::new(),
            }),
            "SwitchSession",
        ),
    ];
    assert_eq!(command_cases.len(), 23);
    for (command, command_name) in &command_cases {
        assert_eq!(&get_variant_name(command), command_name);
    }
}

#[test]
fn visual_variant_names_are_canonical() {
    let visual_command_cases: Vec<(VisualCommand, &str)> = vec![
        (
            VisualCommand::SetSelection(SetSelectionArgs {
                pane_id: PaneId::new(),
                selection: Selection {
                    selection_kind: SelectionKind::Character,
                    anchor: GridPosition {
                        row_index: 0,
                        column_index: 0,
                    },
                    cursor: GridPosition {
                        row_index: 0,
                        column_index: 1,
                    },
                },
            }),
            "SetSelection",
        ),
        (
            VisualCommand::ClearSelection(ClearSelectionArgs {
                pane_id: PaneId::new(),
            }),
            "ClearSelection",
        ),
        (
            VisualCommand::Copy(CopyArgs {
                pane_id: PaneId::new(),
                should_trim_trailing_whitespace: true,
                clipboard_target: CopyTarget::Osc52,
            }),
            "Copy",
        ),
    ];
    assert_eq!(visual_command_cases.len(), 3);
    for (visual_command, command_name) in &visual_command_cases {
        assert_eq!(&get_variant_name(visual_command), command_name);
    }
}

/// `Command::kind` reports the matching discriminant for every variant, and
/// every `CommandKind` round-trips through JSON.
#[test]
fn command_kind_mirrors_command() {
    let command_kind_cases: Vec<(Command, CommandKind)> = vec![
        (
            Command::NewPane(build_new_pane_args()),
            CommandKind::NewPane,
        ),
        (
            Command::ClosePane(ClosePaneArgs::default()),
            CommandKind::ClosePane,
        ),
        (
            Command::ResizePane(ResizePaneArgs {
                pane_id: None,
                direction: Direction::Up,
                resize_amount_cells: 1,
            }),
            CommandKind::ResizePane,
        ),
        (
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Pane(PaneId::new()),
                client_id: None,
            }),
            CommandKind::FocusPane,
        ),
        (Command::NewTab(NewTabArgs::default()), CommandKind::NewTab),
        (
            Command::CloseTab(CloseTabArgs::default()),
            CommandKind::CloseTab,
        ),
        (
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Next,
                client_id: None,
            }),
            CommandKind::FocusTab,
        ),
        (
            Command::WriteToPane(WriteToPaneArgs::default()),
            CommandKind::WriteToPane,
        ),
        (
            Command::ToggleLockMode(ToggleLockModeArgs::default()),
            CommandKind::ToggleLockMode,
        ),
        (
            Command::SetLockMode(LockModeArgs {
                is_locked: true,
                client_id: None,
            }),
            CommandKind::SetLockMode,
        ),
        (
            Command::RunCommandPane(RunCommandPaneArgs {
                spawn_spec: SpawnSpec {
                    program: std::path::PathBuf::from("ls"),
                    arguments: vec![],
                    working_directory: None,
                    environment_variables: std::collections::BTreeMap::new(),
                    shell_kind: crate::process::ShellKind::Other("x".to_string()),
                },
                working_directory: None,
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Right,
                should_stack: false,
                client_id: None,
            }),
            CommandKind::RunCommandPane,
        ),
        (
            Command::Visual(VisualCommand::ClearSelection(ClearSelectionArgs {
                pane_id: PaneId::new(),
            })),
            CommandKind::Visual,
        ),
        (
            Command::Plugin(PluginCommand::Reload(ReloadPluginArgs {
                plugin_id: PluginId::new(),
            })),
            CommandKind::Plugin,
        ),
        (
            Command::TogglePaneFullscreen,
            CommandKind::TogglePaneFullscreen,
        ),
        (Command::ToggleMouseSelect, CommandKind::ToggleMouseSelect),
        (
            Command::MoveTab(MoveTabArgs {
                tab_id: None,
                target_tab_index: 0,
            }),
            CommandKind::MoveTab,
        ),
        (
            Command::MovePane(MovePaneArgs {
                pane_id: None,
                direction: Direction::Left,
            }),
            CommandKind::MovePane,
        ),
        (
            Command::SwapPanes(SwapPanesArgs {
                source_pane_id: None,
                target_pane_id: PaneId::new(),
            }),
            CommandKind::SwapPanes,
        ),
        (
            Command::ScrollPane(ScrollPaneArgs {
                pane_id: None,
                scroll_line_count: 1,
            }),
            CommandKind::ScrollPane,
        ),
        (Command::Quit, CommandKind::Quit),
        (Command::Detach(DetachArgs::default()), CommandKind::Detach),
        (Command::DetachAll, CommandKind::DetachAll),
        (
            Command::SwitchSession(SwitchSessionArgs {
                client_id: None,
                session_id: SessionId::new(),
            }),
            CommandKind::SwitchSession,
        ),
    ];
    assert_eq!(command_kind_cases.len(), 23);
    for (command, command_kind) in &command_kind_cases {
        assert_eq!(command.get_command_kind(), *command_kind);
        assert_json_roundtrip(command_kind);
    }
}

/// A fixed timestamp so envelope roundtrips stay deterministic.
fn build_fixed_timestamp() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

#[test]
fn command_source_variants_roundtrip() {
    assert_json_roundtrip(&CommandSource::KeyBinding {
        client_id: ClientId::new(),
    });
    assert_json_roundtrip(&CommandSource::Mouse {
        client_id: ClientId::new(),
    });
    assert_json_roundtrip(&CommandSource::InSessionCli {
        session_id: SessionId::new(),
        client_id: Some(ClientId::new()),
        pane_id: PaneId::new(),
        socket_path: PathBuf::from("/run/koshi/session.sock"),
    });
    assert_json_roundtrip(&CommandSource::InSessionCli {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
        socket_path: PathBuf::from("/run/koshi/session.sock"),
    });
    assert_json_roundtrip(&CommandSource::ExternalCli {
        session_id: Some(SessionId::new()),
        target_client_id: None,
    });
    assert_json_roundtrip(&CommandSource::ExternalCli {
        session_id: None,
        target_client_id: None,
    });
    assert_json_roundtrip(&CommandSource::Plugin {
        plugin_id: PluginId::new(),
    });
    assert_json_roundtrip(&CommandSource::Internal);
}

#[test]
fn command_envelope_roundtrips() {
    assert_json_roundtrip(&CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::InSessionCli {
            session_id: SessionId::new(),
            client_id: Some(ClientId::new()),
            pane_id: PaneId::new(),
            socket_path: PathBuf::from("/run/koshi/session.sock"),
        },
        build_fixed_timestamp(),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    ));
}

#[test]
fn envelope_client_id_mirrors_source() {
    let client_id = ClientId::new();
    let with_client = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        build_fixed_timestamp(),
        Command::TogglePaneFullscreen,
    );
    assert_eq!(with_client.client_id, Some(client_id));

    let without_client = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::Internal,
        build_fixed_timestamp(),
        Command::TogglePaneFullscreen,
    );
    assert_eq!(without_client.client_id, None);
}

#[test]
fn command_source_variant_names_are_canonical() {
    let command_source_cases: Vec<(CommandSource, &str)> = vec![
        (
            CommandSource::KeyBinding {
                client_id: ClientId::new(),
            },
            "KeyBinding",
        ),
        (
            CommandSource::Mouse {
                client_id: ClientId::new(),
            },
            "Mouse",
        ),
        (
            CommandSource::InSessionCli {
                session_id: SessionId::new(),
                client_id: Some(ClientId::new()),
                pane_id: PaneId::new(),
                socket_path: PathBuf::from("/run/koshi/session.sock"),
            },
            "InSessionCli",
        ),
        (
            CommandSource::ExternalCli {
                session_id: None,
                target_client_id: None,
            },
            "ExternalCli",
        ),
        (
            CommandSource::Plugin {
                plugin_id: PluginId::new(),
            },
            "Plugin",
        ),
        (CommandSource::Internal, "Internal"),
    ];
    assert_eq!(command_source_cases.len(), 6);
    for (command_source, source_name) in &command_source_cases {
        assert_eq!(&get_variant_name(command_source), source_name);
    }
}

#[test]
fn envelope_from_a_clientless_in_session_cli_carries_no_client() {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_in_session_cli(
            SessionId::new(),
            None,
            PaneId::new(),
            PathBuf::from("/run/koshi/session.sock"),
        ),
        build_fixed_timestamp(),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert_eq!(command_envelope.client_id, None);
    assert_eq!(
        command_envelope.clone().validate_command_envelope(),
        Ok(command_envelope)
    );
}

#[test]
fn deserialize_rejects_a_forged_client_on_a_clientless_in_session_cli() {
    // The source names no client; the wire claims one.
    let forged_envelope = CommandEnvelope {
        command_id: CommandId::new(),
        command_source: CommandSource::from_in_session_cli(
            SessionId::new(),
            None,
            PaneId::new(),
            PathBuf::from("/run/koshi/session.sock"),
        ),
        client_id: Some(ClientId::new()),
        issued_at: build_fixed_timestamp(),
        command: Command::ToggleLockMode(ToggleLockModeArgs::default()),
    };
    let envelope_json = serde_json::to_value(&forged_envelope).expect("serialize");
    let validation_error =
        serde_json::from_value::<CommandEnvelope>(envelope_json).expect_err("rejects");
    assert_eq!(
        validation_error.to_string(),
        "envelope client_id does not match its source"
    );
}

#[test]
fn deserialize_rejects_client_id_mismatch() {
    // The `Internal` source names no client; the wire claims one.
    let forged_envelope = CommandEnvelope {
        command_id: CommandId::new(),
        command_source: CommandSource::Internal,
        client_id: Some(ClientId::new()),
        issued_at: build_fixed_timestamp(),
        command: Command::ToggleLockMode(ToggleLockModeArgs::default()),
    };
    let envelope_json = serde_json::to_value(&forged_envelope).expect("serialize");
    let validation_error =
        serde_json::from_value::<CommandEnvelope>(envelope_json).expect_err("rejects");
    assert_eq!(
        validation_error.to_string(),
        "envelope client_id does not match its source"
    );
}

#[test]
fn validate_command_envelope_rejects_client_id_mismatch() {
    let forged = CommandEnvelope {
        command_id: CommandId::new(),
        command_source: CommandSource::KeyBinding {
            client_id: ClientId::new(),
        },
        client_id: Some(ClientId::new()), // a different client than the source
        issued_at: build_fixed_timestamp(),
        command: Command::ToggleLockMode(ToggleLockModeArgs::default()),
    };
    assert_eq!(
        forged.validate_command_envelope(),
        Err(CommandEnvelopeError::ClientIdMismatch)
    );
}

#[test]
fn validate_command_envelope_accepts_consistent_envelope() {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::Internal,
        build_fixed_timestamp(),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert_eq!(
        command_envelope.clone().validate_command_envelope(),
        Ok(command_envelope)
    );
}

#[test]
fn command_envelope_error_message_is_human() {
    assert_eq!(
        CommandEnvelopeError::ClientIdMismatch.to_string(),
        "envelope client_id does not match its source"
    );
}

#[test]
fn deserialize_rejects_a_missing_client_id_when_the_source_names_one() {
    // The source names a client (`KeyBinding`); the wire `client_id` is `null`.
    let valid_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding {
            client_id: ClientId::new(),
        },
        build_fixed_timestamp(),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let mut envelope_json = serde_json::to_value(&valid_envelope).expect("serialize");
    envelope_json["client_id"] = serde_json::Value::Null;

    let validation_error =
        serde_json::from_value::<CommandEnvelope>(envelope_json).expect_err("rejects");
    assert_eq!(
        validation_error.to_string(),
        "envelope client_id does not match its source"
    );
}

#[test]
fn reject_reason_roundtrips() {
    assert_json_roundtrip(&RejectReason::TargetGone);
    assert_json_roundtrip(&RejectReason::TargetAmbiguous);
    assert_json_roundtrip(&RejectReason::TargetNotFound);
    assert_json_roundtrip(&RejectReason::SourceClientStale);
    assert_json_roundtrip(&RejectReason::Unauthorized);
    assert_json_roundtrip(&RejectReason::InvalidState);
    assert_json_roundtrip(&RejectReason::MinSize);
}

#[test]
fn command_result_roundtrips() {
    assert_json_roundtrip(&CommandResult::Ok {
        command_id: CommandId::new(),
        emitted_events: vec![
            Event::Quit(QuitCause::Requested),
            Event::Quit(QuitCause::Requested),
        ],
    });
    assert_json_roundtrip(&CommandResult::Rejected {
        command_id: CommandId::new(),
        reason: RejectReason::TargetNotFound,
        help: Some("pass an explicit --pane id".to_string()),
    });
    assert_json_roundtrip(&CommandResult::Rejected {
        command_id: CommandId::new(),
        reason: RejectReason::MinSize,
        help: None,
    });
}

/// Every reason produces a human string. Pins the diagnostic helper to the
/// real variant set; any added/renamed reason breaks this.
#[test]
fn reject_reason_diagnostics_are_human() {
    let reject_reason_cases: Vec<(RejectReason, &str)> = vec![
        (RejectReason::TargetGone, "target no longer exists"),
        (
            RejectReason::TargetAmbiguous,
            "target matched more than one; specify an explicit id",
        ),
        (RejectReason::TargetNotFound, "no target matched"),
        (
            RejectReason::SourceClientStale,
            "source client has detached",
        ),
        (RejectReason::Unauthorized, "command not permitted"),
        (RejectReason::InvalidState, "invalid in the current state"),
        (RejectReason::MinSize, "below minimum size"),
    ];
    assert_eq!(reject_reason_cases.len(), 7);
    for (reject_reason, expected_reject_reason_text) in &reject_reason_cases {
        assert_eq!(&reject_reason.to_string(), expected_reject_reason_text);
    }
}

#[test]
fn cli_exit_codes_match_spec() {
    assert_eq!(CliExitCode::Success.get_exit_code(), 0);
    assert_eq!(CliExitCode::RuntimeAction.get_exit_code(), 1);
    assert_eq!(CliExitCode::UsageOrConfig.get_exit_code(), 2);
    assert_eq!(CliExitCode::SessionNotFound.get_exit_code(), 3);
    assert_eq!(CliExitCode::IpcUnavailable.get_exit_code(), 4);
}

#[test]
fn toggle_pane_fullscreen_is_a_bare_wire_string() {
    // The byte shape a still-running 0.3.0 session decodes: a unit variant
    // carries no object, only its name.
    assert_eq!(
        serde_json::to_string(&Command::TogglePaneFullscreen).unwrap(),
        "\"TogglePaneFullscreen\""
    );
    assert_eq!(
        serde_json::from_str::<Command>("\"TogglePaneFullscreen\"").unwrap(),
        Command::TogglePaneFullscreen
    );
}

#[test]
fn an_external_cli_source_without_a_client_still_decodes() {
    // JSON carrying no `target_client_id` field decodes with it `None`.
    assert_eq!(
        serde_json::from_str::<CommandSource>(r#"{"ExternalCli":{"session_id":null}}"#).unwrap(),
        CommandSource::ExternalCli {
            session_id: None,
            target_client_id: None,
        }
    );

    let session_id = SessionId::new();
    let uuid = session_id.get_uuid();
    let source_json = format!(r#"{{"ExternalCli":{{"session_id":"{uuid}"}}}}"#);
    assert_eq!(
        serde_json::from_str::<CommandSource>(&source_json).unwrap(),
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        }
    );
}

#[test]
fn an_older_build_ignores_the_target_client() {
    /// The `ExternalCli` shape 0.3.0 decodes: a session target and nothing else.
    #[derive(Deserialize, PartialEq, Debug)]
    enum OldSource {
        ExternalCli { session_id: Option<SessionId> },
    }

    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let serialized_command_source = serde_json::to_string(&CommandSource::from_external_cli(
        Some(session_id),
        Some(client_id),
    ))
    .expect("serialize");

    assert_eq!(
        serde_json::from_str::<OldSource>(&serialized_command_source).expect("deserialize"),
        OldSource::ExternalCli {
            session_id: Some(session_id),
        }
    );
}

#[test]
fn the_target_client_is_never_the_acting_client() {
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let pane_id = PaneId::new();
    let socket_path = PathBuf::from("/run/koshi/session.sock");

    let targeted_command_source =
        CommandSource::from_external_cli(Some(session_id), Some(client_id));
    assert_eq!(
        targeted_command_source.get_target_client_id(),
        Some(client_id)
    );
    assert_eq!(targeted_command_source.get_client_id(), None);

    assert_eq!(
        CommandSource::from_external_cli(Some(session_id), None).get_target_client_id(),
        None
    );
    assert_eq!(
        CommandSource::from_in_session_cli(session_id, Some(client_id), pane_id, socket_path)
            .get_target_client_id(),
        None
    );
    assert_eq!(
        CommandSource::KeyBinding { client_id }.get_target_client_id(),
        None
    );
    assert_eq!(
        CommandSource::Mouse { client_id }.get_target_client_id(),
        None
    );
    assert_eq!(
        CommandSource::Plugin {
            plugin_id: PluginId::new(),
        }
        .get_target_client_id(),
        None
    );
    assert_eq!(CommandSource::Internal.get_target_client_id(), None);

    let envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_external_cli(Some(session_id), Some(client_id)),
        build_fixed_timestamp(),
        Command::TogglePaneFullscreen,
    );
    assert_eq!(envelope.client_id, None);
    envelope
        .validate_command_envelope()
        .expect("a source naming a target client is a well-formed envelope");
}

/// The `RunCommandPane` request that spawns `ls` with nothing else chosen.
fn run_ls_args() -> RunCommandPaneArgs {
    RunCommandPaneArgs {
        spawn_spec: SpawnSpec {
            program: std::path::PathBuf::from("ls"),
            arguments: vec![],
            working_directory: None,
            environment_variables: std::collections::BTreeMap::new(),
            shell_kind: crate::process::ShellKind::Other("ls".to_string()),
        },
        working_directory: None,
        source_pane_id: None,
        tab_id: None,
        direction: Direction::Right,
        should_stack: false,
        client_id: None,
    }
}

#[test]
fn client_id_names_the_issuer_for_key_binding_mouse_and_in_session_cli_only() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let pane_id = PaneId::new();
    let socket_path = PathBuf::from("/run/koshi/session.sock");

    assert_eq!(
        CommandSource::KeyBinding { client_id }.get_client_id(),
        Some(client_id)
    );
    assert_eq!(
        CommandSource::Mouse { client_id }.get_client_id(),
        Some(client_id)
    );
    assert_eq!(
        CommandSource::from_in_session_cli(
            session_id,
            Some(client_id),
            pane_id,
            socket_path.clone()
        )
        .get_client_id(),
        Some(client_id)
    );
    assert_eq!(
        CommandSource::from_in_session_cli(session_id, None, pane_id, socket_path).get_client_id(),
        None
    );
    assert_eq!(
        CommandSource::from_external_cli(Some(session_id), Some(client_id)).get_client_id(),
        None
    );
    assert_eq!(
        CommandSource::Plugin {
            plugin_id: PluginId::new(),
        }
        .get_client_id(),
        None
    );
    assert_eq!(CommandSource::Internal.get_client_id(), None);
}

#[test]
fn source_constructors_build_the_matching_variant() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let pane_id = PaneId::new();
    let plugin_id = PluginId::new();
    let socket_path = PathBuf::from("/run/koshi/session.sock");

    assert_eq!(
        CommandSource::from_key_binding(client_id),
        CommandSource::KeyBinding { client_id }
    );
    assert_eq!(
        CommandSource::from_mouse(client_id),
        CommandSource::Mouse { client_id }
    );
    assert_eq!(
        CommandSource::from_in_session_cli(
            session_id,
            Some(client_id),
            pane_id,
            socket_path.clone()
        ),
        CommandSource::InSessionCli {
            session_id,
            client_id: Some(client_id),
            pane_id,
            socket_path,
        }
    );
    assert_eq!(
        CommandSource::from_external_cli(Some(session_id), Some(client_id)),
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: Some(client_id),
        }
    );
    assert_eq!(
        CommandSource::from_plugin(plugin_id),
        CommandSource::Plugin { plugin_id }
    );
}

#[test]
fn from_parts_derives_the_client_from_every_source() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let pane_id = PaneId::new();
    let socket_path = PathBuf::from("/run/koshi/session.sock");

    let command_source_cases: Vec<(CommandSource, Option<ClientId>)> = vec![
        (CommandSource::from_key_binding(client_id), Some(client_id)),
        (CommandSource::from_mouse(client_id), Some(client_id)),
        (
            CommandSource::from_in_session_cli(
                session_id,
                Some(client_id),
                pane_id,
                socket_path.clone(),
            ),
            Some(client_id),
        ),
        (
            CommandSource::from_in_session_cli(session_id, None, pane_id, socket_path),
            None,
        ),
        (
            CommandSource::from_external_cli(Some(session_id), Some(client_id)),
            None,
        ),
        (CommandSource::from_plugin(PluginId::new()), None),
        (CommandSource::Internal, None),
    ];
    assert_eq!(command_source_cases.len(), 7);
    for (command_source, expected_client_id) in command_source_cases {
        let command_envelope = CommandEnvelope::from_parts(
            CommandId::new(),
            command_source.clone(),
            build_fixed_timestamp(),
            Command::Quit,
        );
        assert_eq!(
            command_envelope.client_id, expected_client_id,
            "{command_source:?}"
        );
        assert_eq!(command_envelope.command_source, command_source);
        assert_eq!(command_envelope.issued_at, build_fixed_timestamp());
        assert_eq!(command_envelope.command, Command::Quit);
    }
}

#[test]
fn validate_command_envelope_returns_an_envelope_whose_client_matches_its_source_unchanged() {
    let client_id = ClientId::new();
    let command_envelope = CommandEnvelope {
        command_id: CommandId::new(),
        command_source: CommandSource::from_mouse(client_id),
        client_id: Some(client_id),
        issued_at: build_fixed_timestamp(),
        command: Command::TogglePaneFullscreen,
    };

    assert_eq!(
        command_envelope.clone().validate_command_envelope(),
        Ok(command_envelope)
    );
}

#[test]
fn validate_command_envelope_rejects_a_missing_client_when_the_source_names_one() {
    let command_envelope = CommandEnvelope {
        command_id: CommandId::new(),
        command_source: CommandSource::from_mouse(ClientId::new()),
        client_id: None,
        issued_at: build_fixed_timestamp(),
        command: Command::TogglePaneFullscreen,
    };

    assert_eq!(
        command_envelope.validate_command_envelope(),
        Err(CommandEnvelopeError::ClientIdMismatch)
    );
}

#[test]
fn an_envelope_written_without_a_client_id_field_decodes_when_its_source_names_none() {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::Internal,
        build_fixed_timestamp(),
        Command::Quit,
    );
    let mut envelope_json = serde_json::to_value(&command_envelope).expect("serialize");
    envelope_json
        .as_object_mut()
        .expect("an envelope is a JSON object")
        .remove("client_id")
        .expect("the envelope carries a `client_id` field to remove");

    let decoded_envelope: CommandEnvelope =
        serde_json::from_value(envelope_json).expect("deserialize");

    assert_eq!(decoded_envelope, command_envelope);
}

#[test]
fn an_envelope_written_without_a_client_id_field_is_rejected_when_its_source_names_one() {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(ClientId::new()),
        build_fixed_timestamp(),
        Command::Quit,
    );
    let mut envelope_json = serde_json::to_value(&command_envelope).expect("serialize");
    envelope_json
        .as_object_mut()
        .expect("an envelope is a JSON object")
        .remove("client_id")
        .expect("the envelope carries a `client_id` field to remove");

    let validation_error =
        serde_json::from_value::<CommandEnvelope>(envelope_json).expect_err("rejects");

    assert_eq!(
        validation_error.to_string(),
        "envelope client_id does not match its source"
    );
}

#[test]
fn an_envelope_written_without_its_command_is_rejected() {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::Internal,
        build_fixed_timestamp(),
        Command::Quit,
    );
    let mut envelope_json = serde_json::to_value(&command_envelope).expect("serialize");
    envelope_json
        .as_object_mut()
        .expect("an envelope is a JSON object")
        .remove("command")
        .expect("the envelope carries a `command` field to remove");

    let missing_command_error =
        serde_json::from_value::<CommandEnvelope>(envelope_json).expect_err("rejects");

    assert_eq!(missing_command_error.to_string(), "missing field `command`");
}

#[test]
fn command_kind_serializes_as_its_variant_name() {
    let command_kinds = [
        CommandKind::NewPane,
        CommandKind::ClosePane,
        CommandKind::ResizePane,
        CommandKind::FocusPane,
        CommandKind::NewTab,
        CommandKind::CloseTab,
        CommandKind::FocusTab,
        CommandKind::WriteToPane,
        CommandKind::ToggleLockMode,
        CommandKind::SetLockMode,
        CommandKind::ToggleMouseSelect,
        CommandKind::RunCommandPane,
        CommandKind::Visual,
        CommandKind::Plugin,
        CommandKind::TogglePaneFullscreen,
        CommandKind::MoveTab,
        CommandKind::MovePane,
        CommandKind::SwapPanes,
        CommandKind::ScrollPane,
        CommandKind::Quit,
        CommandKind::Detach,
        CommandKind::DetachAll,
        CommandKind::SwitchSession,
    ];
    assert_eq!(command_kinds.len(), 23);
    for command_kind in command_kinds {
        assert_eq!(
            serde_json::to_value(command_kind).expect("serialize"),
            json!(get_variant_name(&command_kind))
        );
    }
}

#[test]
fn every_payload_free_command_is_a_bare_wire_string() {
    for (command, expected_wire_text) in [
        (Command::ToggleMouseSelect, "\"ToggleMouseSelect\""),
        (Command::TogglePaneFullscreen, "\"TogglePaneFullscreen\""),
        (Command::Quit, "\"Quit\""),
        (Command::DetachAll, "\"DetachAll\""),
    ] {
        assert_eq!(
            serde_json::to_string(&command).expect("serialize"),
            expected_wire_text
        );
        assert_eq!(
            serde_json::from_str::<Command>(expected_wire_text).expect("deserialize"),
            command
        );
    }
}

#[test]
fn plugin_commands_roundtrip_every_variant() {
    let plugin_id = PluginId::new();
    assert_json_roundtrip(&PluginCommand::Install(InstallPluginArgs {
        plugin_source: "./local/plugin.wasm".to_string(),
    }));
    assert_json_roundtrip(&PluginCommand::Uninstall(UninstallPluginArgs { plugin_id }));
    assert_json_roundtrip(&PluginCommand::Enable(EnablePluginArgs { plugin_id }));
    assert_json_roundtrip(&PluginCommand::Disable(DisablePluginArgs { plugin_id }));
    assert_json_roundtrip(&PluginCommand::Update(UpdatePluginArgs { plugin_id }));
    assert_json_roundtrip(&PluginCommand::Reload(ReloadPluginArgs { plugin_id }));
}

#[test]
fn focus_targets_and_tab_targets_roundtrip_every_variant() {
    assert_json_roundtrip(&FocusTarget::Pane(PaneId::new()));
    assert_json_roundtrip(&FocusTarget::Direction(Direction::Down));
    assert_json_roundtrip(&TabTarget::Next);
    assert_json_roundtrip(&TabTarget::Prev);
    assert_json_roundtrip(&TabTarget::Index(0));
    assert_json_roundtrip(&TabTarget::Index(usize::MAX));
    assert_json_roundtrip(&TabTarget::Id(TabId::new()));
}

#[test]
fn selection_kinds_and_copy_targets_roundtrip_every_variant() {
    assert_json_roundtrip(&SelectionKind::Character);
    assert_json_roundtrip(&SelectionKind::Word);
    assert_json_roundtrip(&SelectionKind::Line);
    assert_json_roundtrip(&SelectionKind::Block);
    assert_json_roundtrip(&CopyTarget::Osc52);
    assert_json_roundtrip(&CopyTarget::Native);
}

#[test]
fn extreme_numeric_fields_roundtrip() {
    assert_json_roundtrip(&ResizePaneArgs {
        pane_id: None,
        direction: Direction::Left,
        resize_amount_cells: i16::MIN,
    });
    assert_json_roundtrip(&ResizePaneArgs {
        pane_id: None,
        direction: Direction::Right,
        resize_amount_cells: i16::MAX,
    });
    assert_json_roundtrip(&GridPosition {
        row_index: u64::MAX,
        column_index: u16::MAX,
    });
    assert_json_roundtrip(&GridPosition {
        row_index: 0,
        column_index: 0,
    });
    assert_json_roundtrip(&MoveTabArgs {
        tab_id: None,
        target_tab_index: usize::MAX,
    });
}

#[test]
fn a_resize_size_past_i16_is_rejected() {
    let parse_error = serde_json::from_value::<ResizePaneArgs>(json!({
        "pane_id": null,
        "direction": "Left",
        "resize_amount_cells": 32768
    }))
    .expect_err("rejects");

    assert_eq!(
        parse_error.to_string(),
        "invalid value: integer `32768`, expected i16"
    );
}

#[test]
fn write_to_pane_carries_every_byte_value() {
    let input_bytes: Vec<u8> = (0..=255).collect();
    assert_json_roundtrip(&WriteToPaneArgs {
        pane_id: Some(PaneId::new()),
        input_bytes: input_bytes.clone(),
    });

    let write_to_pane_json = serde_json::to_value(WriteToPaneArgs {
        pane_id: None,
        input_bytes,
    })
    .expect("serialize");
    assert_eq!(write_to_pane_json["input_bytes"][0], json!(0));
    assert_eq!(write_to_pane_json["input_bytes"][255], json!(255));
    assert_eq!(
        write_to_pane_json["input_bytes"].as_array().map(Vec::len),
        Some(256)
    );
}

#[test]
fn args_written_without_their_defaulted_fields_still_decode() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let client_json = serde_json::to_value(client_id).expect("serialize");
    let session_json = serde_json::to_value(session_id).expect("serialize");

    assert_eq!(
        serde_json::from_value::<ClosePaneArgs>(
            json!({"pane_id": null, "should_force_close": true})
        )
        .expect("deserialize"),
        ClosePaneArgs {
            pane_id: None,
            should_force_close: true,
            should_kill_process_tree: false,
        }
    );
    assert_eq!(
        serde_json::from_value::<CloseTabArgs>(
            json!({"tab_id": null, "should_force_close": false})
        )
        .expect("deserialize"),
        CloseTabArgs {
            tab_id: None,
            should_force_close: false,
            should_kill_process_tree: false,
        }
    );
    assert_eq!(
        serde_json::from_value::<NewPaneArgs>(json!({
            "source_pane_id": null,
            "tab_id": null,
            "direction": "Right",
            "should_stack": false,
            "working_directory": null,
            "spawn_spec": null,
            "client_id": null
        }))
        .expect("deserialize"),
        build_new_pane_args()
    );
    assert_eq!(
        serde_json::from_value::<LockModeArgs>(json!({"is_locked": true})).expect("deserialize"),
        LockModeArgs {
            is_locked: true,
            client_id: None,
        }
    );
    assert_eq!(
        serde_json::from_value::<ToggleLockModeArgs>(json!({})).expect("deserialize"),
        ToggleLockModeArgs { client_id: None }
    );
    assert_eq!(
        serde_json::from_value::<DetachArgs>(json!({})).expect("deserialize"),
        DetachArgs { client_id: None }
    );
    assert_eq!(
        serde_json::from_value::<SwitchSessionArgs>(json!({"session_id": session_json}))
            .expect("deserialize"),
        SwitchSessionArgs {
            client_id: None,
            session_id,
        }
    );
    assert_eq!(
        serde_json::from_value::<LockModeArgs>(
            json!({"is_locked": false, "client_id": client_json})
        )
        .expect("deserialize"),
        LockModeArgs {
            is_locked: false,
            client_id: Some(client_id),
        }
    );
}

#[test]
fn run_command_pane_args_written_without_tab_and_client_still_decode() {
    let mut command_args_json = serde_json::to_value(run_ls_args()).expect("serialize");
    let command_fields = command_args_json
        .as_object_mut()
        .expect("args are a JSON object");
    command_fields
        .remove("tab_id")
        .expect("the args carry a `tab_id` field to remove");
    command_fields
        .remove("client_id")
        .expect("the args carry a `client_id` field to remove");

    let decoded_run_command_args: RunCommandPaneArgs =
        serde_json::from_value(command_args_json).expect("deserialize");

    assert_eq!(decoded_run_command_args, run_ls_args());
}

#[test]
fn a_command_with_an_unknown_variant_name_is_rejected() {
    let parse_error = serde_json::from_value::<Command>(json!("Reboot")).expect_err("rejects");

    assert_eq!(
        parse_error.to_string(),
        "unknown variant `Reboot`, expected one of `NewPane`, `ClosePane`, `ResizePane`, `FocusPane`, `NewTab`, `CloseTab`, `FocusTab`, `WriteToPane`, `ToggleLockMode`, `SetLockMode`, `ToggleMouseSelect`, `RunCommandPane`, `Visual`, `Plugin`, `TogglePaneFullscreen`, `MoveTab`, `MovePane`, `SwapPanes`, `ScrollPane`, `Quit`, `Detach`, `DetachAll`, `SwitchSession`"
    );
}

#[test]
fn a_command_result_with_an_unknown_variant_name_is_rejected() {
    let parse_error =
        serde_json::from_value::<CommandResult>(json!({"Pending": {}})).expect_err("rejects");

    assert_eq!(
        parse_error.to_string(),
        "unknown variant `Pending`, expected `Ok` or `Rejected`"
    );
}

#[test]
fn command_envelope_error_implements_std_error() {
    let envelope_error: Box<dyn std::error::Error> =
        Box::new(CommandEnvelopeError::ClientIdMismatch);

    assert_eq!(
        envelope_error.to_string(),
        "envelope client_id does not match its source"
    );
}
