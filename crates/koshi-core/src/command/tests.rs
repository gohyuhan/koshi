//! Tests for command serialization, variant canonicality, and validation.
//!
//! Covers roundtripping commands through JSON, verifying variant names and
//! discriminants are stable, and ensuring command envelopes validate client IDs.

use super::*;
use crate::event::RejectReason;
use crate::ids::{ClientId, CommandId, EventId, PaneId, PluginId, SessionId};
use std::time::{Duration, UNIX_EPOCH};

/// Roundtrip a value through JSON and assert it survives unchanged.
fn roundtrip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serialize");
    let back: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(*value, back);
}

#[test]
fn unit_commands_roundtrip() {
    roundtrip(&Command::ToggleLockMode);
    roundtrip(&Command::TogglePaneFullscreen);
    roundtrip(&Command::Quit);
}

#[test]
fn pane_commands_roundtrip() {
    roundtrip(&Command::NewPane(NewPaneArgs {
        direction: Some(Direction::Right),
        client: Some(ClientId::new()),
        ..NewPaneArgs::default()
    }));
    roundtrip(&Command::ClosePane(ClosePaneArgs {
        pane: Some(PaneId::new()),
        force: true,
        tree: true,
    }));
    roundtrip(&Command::ResizePane(ResizePaneArgs {
        pane: None,
        direction: Direction::Up,
        size: 4,
    }));
    roundtrip(&Command::ResizePane(ResizePaneArgs {
        pane: Some(PaneId::new()),
        direction: Direction::Left,
        size: -3,
    }));
    roundtrip(&Command::RunCommandPane(RunCommandPaneArgs {
        command: SpawnSpec {
            program: std::path::PathBuf::from("htop"),
            args: vec!["-d".to_string()],
            cwd: None,
            env: std::collections::BTreeMap::new(),
            shell_kind: crate::process::ShellKind::Other("htop".to_string()),
        },
        cwd: None,
        source: Some(PaneId::new()),
        direction: Some(Direction::Down),
        stacked: false,
    }));
    roundtrip(&Command::FocusPane(FocusPaneArgs {
        target: FocusTarget::Pane(PaneId::new()),
        client: None,
    }));
    roundtrip(&Command::FocusPane(FocusPaneArgs {
        target: FocusTarget::Pane(PaneId::new()),
        client: Some(ClientId::new()),
    }));
    roundtrip(&Command::FocusPane(FocusPaneArgs {
        target: FocusTarget::Direction(Direction::Left),
        client: None,
    }));
    roundtrip(&Command::RenamePane(RenamePaneArgs { pane: None }));
}

#[test]
fn tab_and_session_commands_roundtrip() {
    roundtrip(&Command::FocusTab(FocusTabArgs {
        target: TabTarget::Next,
        client: None,
    }));
    roundtrip(&Command::FocusTab(FocusTabArgs {
        target: TabTarget::Index(2),
        client: None,
    }));
    roundtrip(&Command::MoveTab(MoveTabArgs {
        tab: None,
        index: 0,
    }));
    roundtrip(&Command::RenameSession(RenameSessionArgs {
        session: Some(SessionId::new()),
    }));
}

#[test]
fn write_to_pane_roundtrips() {
    roundtrip(&Command::WriteToPane(WriteToPaneArgs {
        pane: None,
        data: b"ls -la\n".to_vec(),
    }));
}

#[test]
fn visual_commands_roundtrip() {
    roundtrip(&Command::Visual(VisualCommand::SetSelection(
        SetSelectionArgs {
            pane: PaneId::new(),
            selection: Selection {
                kind: SelectionKind::Block,
                anchor: GridPos { row: 10, col: 0 },
                cursor: GridPos { row: 12, col: 40 },
            },
        },
    )));
    roundtrip(&Command::Visual(VisualCommand::ClearSelection(
        ClearSelectionArgs {
            pane: PaneId::new(),
        },
    )));
    roundtrip(&Command::Visual(VisualCommand::Copy(CopyArgs {
        target: CopyTarget::Osc52,
    })));
}

#[test]
fn plugin_commands_roundtrip() {
    roundtrip(&Command::Plugin(PluginCommand::Install(
        InstallPluginArgs {
            source: "https://example.test/p.wasm".to_string(),
        },
    )));
    roundtrip(&Command::Plugin(PluginCommand::Reload(ReloadPluginArgs {
        plugin: PluginId::new(),
    })));
}

/// The variant name from a value's Debug repr: everything before the first `(`
/// (data variants) or the whole string (unit variants). Anchors a name snapshot
/// to the real enum — a rename changes the Debug output and fails the assert.
fn variant_name<T: std::fmt::Debug>(value: &T) -> String {
    let repr = format!("{value:?}");
    let cut = repr.find(['(', '{', ' ']).unwrap_or(repr.len());
    repr[..cut].to_string()
}

/// One instance per top-level variant, paired with its canonical name. Renaming
/// any variant breaks the corresponding `variant_name` assert below, and
/// adding/removing one breaks the count — neither passes on a detached list.
#[test]
fn command_variant_names_are_canonical() {
    let cases: Vec<(Command, &str)> = vec![
        (Command::NewPane(NewPaneArgs::default()), "NewPane"),
        (Command::ClosePane(ClosePaneArgs::default()), "ClosePane"),
        (
            Command::ResizePane(ResizePaneArgs {
                pane: None,
                direction: Direction::Up,
                size: 1,
            }),
            "ResizePane",
        ),
        (
            Command::FocusPane(FocusPaneArgs {
                target: FocusTarget::Pane(PaneId::new()),
                client: None,
            }),
            "FocusPane",
        ),
        (Command::NewTab(NewTabArgs::default()), "NewTab"),
        (Command::CloseTab(CloseTabArgs::default()), "CloseTab"),
        (Command::RenameTab(RenameTabArgs { tab: None }), "RenameTab"),
        (
            Command::FocusTab(FocusTabArgs {
                target: TabTarget::Next,
                client: None,
            }),
            "FocusTab",
        ),
        (
            Command::WriteToPane(WriteToPaneArgs::default()),
            "WriteToPane",
        ),
        (Command::ToggleLockMode, "ToggleLockMode"),
        (
            Command::SetLockMode(LockModeArgs { locked: true }),
            "SetLockMode",
        ),
        (
            Command::RunCommandPane(RunCommandPaneArgs {
                command: SpawnSpec {
                    program: std::path::PathBuf::from("ls"),
                    args: vec![],
                    cwd: None,
                    env: std::collections::BTreeMap::new(),
                    shell_kind: crate::process::ShellKind::Other("x".to_string()),
                },
                cwd: None,
                source: None,
                direction: None,
                stacked: false,
            }),
            "RunCommandPane",
        ),
        (
            Command::Visual(VisualCommand::ClearSelection(ClearSelectionArgs {
                pane: PaneId::new(),
            })),
            "Visual",
        ),
        (
            Command::Plugin(PluginCommand::Reload(ReloadPluginArgs {
                plugin: PluginId::new(),
            })),
            "Plugin",
        ),
        (Command::TogglePaneFullscreen, "TogglePaneFullscreen"),
        (
            Command::RenamePane(RenamePaneArgs { pane: None }),
            "RenamePane",
        ),
        (
            Command::MoveTab(MoveTabArgs {
                tab: None,
                index: 0,
            }),
            "MoveTab",
        ),
        (
            Command::RenameSession(RenameSessionArgs { session: None }),
            "RenameSession",
        ),
        (Command::Quit, "Quit"),
    ];
    assert_eq!(cases.len(), 19);
    for (value, name) in &cases {
        assert_eq!(&variant_name(value), name);
    }
}

#[test]
fn visual_variant_names_are_canonical() {
    // Three variants, not nine: `Enter`/`Exit` are gone because a selection
    // appearing IS entering visual mode, and `MoveCursor` is gone because
    // selecting is the mouse's alone.
    let cases: Vec<(VisualCommand, &str)> = vec![
        (
            VisualCommand::SetSelection(SetSelectionArgs {
                pane: PaneId::new(),
                selection: Selection {
                    kind: SelectionKind::Character,
                    anchor: GridPos { row: 0, col: 0 },
                    cursor: GridPos { row: 0, col: 1 },
                },
            }),
            "SetSelection",
        ),
        (
            VisualCommand::ClearSelection(ClearSelectionArgs {
                pane: PaneId::new(),
            }),
            "ClearSelection",
        ),
        (
            VisualCommand::Copy(CopyArgs {
                target: CopyTarget::Osc52,
            }),
            "Copy",
        ),
    ];
    assert_eq!(cases.len(), 3);
    for (value, name) in &cases {
        assert_eq!(&variant_name(value), name);
    }
}

/// `Command::kind` must report the matching discriminant for every variant.
/// Reusing the canonical command instances keeps `CommandKind` pinned to the
/// same 18-variant set as `Command`; a new command variant added without a
/// `kind` arm fails to compile, and a mismatched arm fails this assert.
#[test]
fn command_kind_mirrors_command() {
    let cases: Vec<(Command, CommandKind)> = vec![
        (
            Command::NewPane(NewPaneArgs::default()),
            CommandKind::NewPane,
        ),
        (
            Command::ClosePane(ClosePaneArgs::default()),
            CommandKind::ClosePane,
        ),
        (
            Command::ResizePane(ResizePaneArgs {
                pane: None,
                direction: Direction::Up,
                size: 1,
            }),
            CommandKind::ResizePane,
        ),
        (
            Command::FocusPane(FocusPaneArgs {
                target: FocusTarget::Pane(PaneId::new()),
                client: None,
            }),
            CommandKind::FocusPane,
        ),
        (Command::NewTab(NewTabArgs::default()), CommandKind::NewTab),
        (
            Command::CloseTab(CloseTabArgs::default()),
            CommandKind::CloseTab,
        ),
        (
            Command::RenameTab(RenameTabArgs { tab: None }),
            CommandKind::RenameTab,
        ),
        (
            Command::FocusTab(FocusTabArgs {
                target: TabTarget::Next,
                client: None,
            }),
            CommandKind::FocusTab,
        ),
        (
            Command::WriteToPane(WriteToPaneArgs::default()),
            CommandKind::WriteToPane,
        ),
        (Command::ToggleLockMode, CommandKind::ToggleLockMode),
        (
            Command::SetLockMode(LockModeArgs { locked: true }),
            CommandKind::SetLockMode,
        ),
        (
            Command::RunCommandPane(RunCommandPaneArgs {
                command: SpawnSpec {
                    program: std::path::PathBuf::from("ls"),
                    args: vec![],
                    cwd: None,
                    env: std::collections::BTreeMap::new(),
                    shell_kind: crate::process::ShellKind::Other("x".to_string()),
                },
                cwd: None,
                source: None,
                direction: None,
                stacked: false,
            }),
            CommandKind::RunCommandPane,
        ),
        (
            Command::Visual(VisualCommand::ClearSelection(ClearSelectionArgs {
                pane: PaneId::new(),
            })),
            CommandKind::Visual,
        ),
        (
            Command::Plugin(PluginCommand::Reload(ReloadPluginArgs {
                plugin: PluginId::new(),
            })),
            CommandKind::Plugin,
        ),
        (
            Command::TogglePaneFullscreen,
            CommandKind::TogglePaneFullscreen,
        ),
        (
            Command::RenamePane(RenamePaneArgs { pane: None }),
            CommandKind::RenamePane,
        ),
        (
            Command::MoveTab(MoveTabArgs {
                tab: None,
                index: 0,
            }),
            CommandKind::MoveTab,
        ),
        (
            Command::RenameSession(RenameSessionArgs { session: None }),
            CommandKind::RenameSession,
        ),
        (Command::Quit, CommandKind::Quit),
    ];
    assert_eq!(cases.len(), 19);
    for (command, kind) in &cases {
        assert_eq!(command.kind(), *kind);
        roundtrip(kind);
    }
}

/// A fixed timestamp so envelope roundtrips stay deterministic.
fn fixed_time() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

#[test]
fn command_source_variants_roundtrip() {
    roundtrip(&CommandSource::KeyBinding {
        client_id: ClientId::new(),
    });
    roundtrip(&CommandSource::Mouse {
        client_id: ClientId::new(),
    });
    roundtrip(&CommandSource::InSessionCli {
        session_id: SessionId::new(),
        client_id: ClientId::new(),
        pane_id: PaneId::new(),
        socket_path: PathBuf::from("/run/koshi/session.sock"),
    });
    roundtrip(&CommandSource::ExternalCli {
        session_id: Some(SessionId::new()),
    });
    roundtrip(&CommandSource::ExternalCli { session_id: None });
    roundtrip(&CommandSource::Plugin {
        plugin_id: PluginId::new(),
    });
    roundtrip(&CommandSource::Internal);
}

#[test]
fn command_envelope_roundtrips() {
    roundtrip(&CommandEnvelope::new(
        CommandId::new(),
        CommandSource::InSessionCli {
            session_id: SessionId::new(),
            client_id: ClientId::new(),
            pane_id: PaneId::new(),
            socket_path: PathBuf::from("/run/koshi/session.sock"),
        },
        fixed_time(),
        Command::ToggleLockMode,
    ));
}

#[test]
fn envelope_client_id_mirrors_source() {
    let client = ClientId::new();
    let with_client = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::KeyBinding { client_id: client },
        fixed_time(),
        Command::TogglePaneFullscreen,
    );
    assert_eq!(with_client.client_id, Some(client));

    let without_client = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::Internal,
        fixed_time(),
        Command::TogglePaneFullscreen,
    );
    assert_eq!(without_client.client_id, None);
}

#[test]
fn command_source_variant_names_are_canonical() {
    let cases: Vec<(CommandSource, &str)> = vec![
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
                client_id: ClientId::new(),
                pane_id: PaneId::new(),
                socket_path: PathBuf::from("/run/koshi/session.sock"),
            },
            "InSessionCli",
        ),
        (
            CommandSource::ExternalCli { session_id: None },
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
    assert_eq!(cases.len(), 6);
    for (value, name) in &cases {
        assert_eq!(&variant_name(value), name);
    }
}

#[test]
fn deserialize_rejects_client_id_mismatch() {
    // Envelope has `Internal` source (which names no client) but claims one on the wire.
    let forged = CommandEnvelope {
        id: CommandId::new(),
        source: CommandSource::Internal,
        client_id: Some(ClientId::new()),
        issued_at: fixed_time(),
        command: Command::ToggleLockMode,
    };
    let json = serde_json::to_string(&forged).expect("serialize");
    let decoded: Result<CommandEnvelope, _> = serde_json::from_str(&json);
    assert!(decoded.is_err(), "mismatched envelope must not deserialize");
}

#[test]
fn validate_rejects_client_id_mismatch() {
    let forged = CommandEnvelope {
        id: CommandId::new(),
        source: CommandSource::KeyBinding {
            client_id: ClientId::new(),
        },
        client_id: Some(ClientId::new()), // a different client than the source
        issued_at: fixed_time(),
        command: Command::ToggleLockMode,
    };
    assert_eq!(
        forged.validate(),
        Err(CommandEnvelopeError::ClientIdMismatch)
    );
}

#[test]
fn validate_accepts_consistent_envelope() {
    let env = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::Internal,
        fixed_time(),
        Command::ToggleLockMode,
    );
    assert!(env.validate().is_ok());
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
    // The mirror case of `deserialize_rejects_client_id_mismatch`: a source
    // that names a client (`KeyBinding`) but a wire `client_id` of `null`
    // (rather than a *different* client) must also fail to decode.
    let valid = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::KeyBinding {
            client_id: ClientId::new(),
        },
        fixed_time(),
        Command::ToggleLockMode,
    );
    let mut value = serde_json::to_value(&valid).expect("serialize");
    value["client_id"] = serde_json::Value::Null;

    let decoded: Result<CommandEnvelope, _> = serde_json::from_value(value);
    assert!(
        decoded.is_err(),
        "an envelope missing its client_id while the source names one must not deserialize"
    );
}

#[test]
fn reject_reason_roundtrips() {
    roundtrip(&RejectReason::TargetGone);
    roundtrip(&RejectReason::TargetAmbiguous);
    roundtrip(&RejectReason::TargetNotFound);
    roundtrip(&RejectReason::SourceClientStale);
    roundtrip(&RejectReason::Unauthorized);
    roundtrip(&RejectReason::InvalidState);
    roundtrip(&RejectReason::MinSize);
}

#[test]
fn command_result_roundtrips() {
    roundtrip(&CommandResult::Ok {
        command_id: CommandId::new(),
        emitted_events: vec![EventId::new(), EventId::new()],
    });
    roundtrip(&CommandResult::Rejected {
        command_id: CommandId::new(),
        reason: RejectReason::TargetNotFound,
        help: Some("pass an explicit --pane id".to_string()),
    });
    roundtrip(&CommandResult::Rejected {
        command_id: CommandId::new(),
        reason: RejectReason::MinSize,
        help: None,
    });
}

/// Every reason produces a human string. Pins the diagnostic helper to the
/// real variant set; any added/renamed reason breaks this.
#[test]
fn reject_reason_diagnostics_are_human() {
    let cases: Vec<(RejectReason, &str)> = vec![
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
    assert_eq!(cases.len(), 7);
    for (reason, expected) in &cases {
        assert_eq!(&reason.to_string(), expected);
    }
}

#[test]
fn cli_exit_codes_match_spec() {
    assert_eq!(CliExitCode::Success.code(), 0);
    assert_eq!(CliExitCode::RuntimeAction.code(), 1);
    assert_eq!(CliExitCode::UsageOrConfig.code(), 2);
    assert_eq!(CliExitCode::SessionNotFound.code(), 3);
    assert_eq!(CliExitCode::IpcUnavailable.code(), 4);
}

#[test]
fn cli_exit_code_maps_command_result() {
    let applied = CommandResult::Ok {
        command_id: CommandId::new(),
        emitted_events: vec![],
    };
    assert_eq!(CliExitCode::for_result(&applied), CliExitCode::Success);

    let rejected = CommandResult::Rejected {
        command_id: CommandId::new(),
        reason: RejectReason::Unauthorized,
        help: None,
    };
    assert_eq!(
        CliExitCode::for_result(&rejected),
        CliExitCode::RuntimeAction
    );
}
