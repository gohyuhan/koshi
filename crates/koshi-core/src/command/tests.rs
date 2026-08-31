//! Tests for command serialization, variant canonicality, and validation.
//!
//! Covers roundtripping commands through JSON, verifying variant names and
//! discriminants are stable, and ensuring command envelopes validate client IDs.

use super::*;
use crate::event::{Event, RejectReason};
use crate::ids::{ClientId, CommandId, PaneId, PluginId, SessionId};
use serde_json::json;
use std::time::{Duration, UNIX_EPOCH};

/// A `new-pane` request with nothing chosen: the focused pane splits rightward.
fn new_pane_args() -> NewPaneArgs {
    NewPaneArgs {
        source: None,
        tab: None,
        direction: Direction::Right,
        stacked: false,
        cwd: None,
        command: None,
        client: None,
    }
}

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
    roundtrip(&Command::ToggleLockMode(ToggleLockModeArgs {
        client: Some(ClientId::new()),
    }));
    roundtrip(&Command::TogglePaneFullscreen);
    roundtrip(&Command::Quit);
}

#[test]
fn pane_commands_roundtrip() {
    roundtrip(&Command::NewPane(NewPaneArgs {
        direction: Direction::Left,
        client: Some(ClientId::new()),
        ..new_pane_args()
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
        tab: Some(TabId::new()),
        direction: Direction::Down,
        stacked: false,
        client: Some(ClientId::new()),
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
        pane: PaneId::new(),
        trim_trailing_whitespace: true,
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

/// The variant name from a value's Debug repr: everything before the first
/// `(`, `{`, or space, or the whole string for a unit variant.
fn variant_name<T: std::fmt::Debug>(value: &T) -> String {
    let repr = format!("{value:?}");
    let cut = repr.find(['(', '{', ' ']).unwrap_or(repr.len());
    repr[..cut].to_string()
}

/// One instance per top-level variant, paired with its canonical name.
#[test]
fn command_variant_names_are_canonical() {
    let cases: Vec<(Command, &str)> = vec![
        (Command::NewPane(new_pane_args()), "NewPane"),
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
        (
            Command::ToggleLockMode(ToggleLockModeArgs::default()),
            "ToggleLockMode",
        ),
        (
            Command::SetLockMode(LockModeArgs {
                locked: true,
                client: None,
            }),
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
                tab: None,
                direction: Direction::Right,
                stacked: false,
                client: None,
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
            Command::MoveTab(MoveTabArgs {
                tab: None,
                index: 0,
            }),
            "MoveTab",
        ),
        (Command::Quit, "Quit"),
        (Command::ToggleMouseSelect, "ToggleMouseSelect"),
        (Command::Detach(DetachArgs::default()), "Detach"),
        (Command::DetachAll, "DetachAll"),
        (
            Command::SwitchSession(SwitchSessionArgs {
                client: None,
                session: SessionId::new(),
            }),
            "SwitchSession",
        ),
    ];
    assert_eq!(cases.len(), 20);
    for (value, name) in &cases {
        assert_eq!(&variant_name(value), name);
    }
}

#[test]
fn visual_variant_names_are_canonical() {
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
                pane: PaneId::new(),
                trim_trailing_whitespace: true,
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

/// `Command::kind` reports the matching discriminant for every variant, and
/// every `CommandKind` round-trips through JSON.
#[test]
fn command_kind_mirrors_command() {
    let cases: Vec<(Command, CommandKind)> = vec![
        (Command::NewPane(new_pane_args()), CommandKind::NewPane),
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
        (
            Command::ToggleLockMode(ToggleLockModeArgs::default()),
            CommandKind::ToggleLockMode,
        ),
        (
            Command::SetLockMode(LockModeArgs {
                locked: true,
                client: None,
            }),
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
                tab: None,
                direction: Direction::Right,
                stacked: false,
                client: None,
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
        (Command::ToggleMouseSelect, CommandKind::ToggleMouseSelect),
        (
            Command::MoveTab(MoveTabArgs {
                tab: None,
                index: 0,
            }),
            CommandKind::MoveTab,
        ),
        (Command::Quit, CommandKind::Quit),
        (Command::Detach(DetachArgs::default()), CommandKind::Detach),
        (Command::DetachAll, CommandKind::DetachAll),
        (
            Command::SwitchSession(SwitchSessionArgs {
                client: None,
                session: SessionId::new(),
            }),
            CommandKind::SwitchSession,
        ),
    ];
    assert_eq!(cases.len(), 20);
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
        client_id: Some(ClientId::new()),
        pane_id: PaneId::new(),
        socket_path: PathBuf::from("/run/koshi/session.sock"),
    });
    roundtrip(&CommandSource::InSessionCli {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
        socket_path: PathBuf::from("/run/koshi/session.sock"),
    });
    roundtrip(&CommandSource::ExternalCli {
        session_id: Some(SessionId::new()),
        target_client: None,
    });
    roundtrip(&CommandSource::ExternalCli {
        session_id: None,
        target_client: None,
    });
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
            client_id: Some(ClientId::new()),
            pane_id: PaneId::new(),
            socket_path: PathBuf::from("/run/koshi/session.sock"),
        },
        fixed_time(),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
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
                client_id: Some(ClientId::new()),
                pane_id: PaneId::new(),
                socket_path: PathBuf::from("/run/koshi/session.sock"),
            },
            "InSessionCli",
        ),
        (
            CommandSource::ExternalCli {
                session_id: None,
                target_client: None,
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
    assert_eq!(cases.len(), 6);
    for (value, name) in &cases {
        assert_eq!(&variant_name(value), name);
    }
}

#[test]
fn envelope_from_a_clientless_in_session_cli_carries_no_client() {
    let env = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::in_session_cli(
            SessionId::new(),
            None,
            PaneId::new(),
            PathBuf::from("/run/koshi/session.sock"),
        ),
        fixed_time(),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert_eq!(env.client_id, None);
    assert_eq!(env.clone().validate(), Ok(env));
}

#[test]
fn deserialize_rejects_a_forged_client_on_a_clientless_in_session_cli() {
    // The source names no client; the wire claims one.
    let forged = CommandEnvelope {
        id: CommandId::new(),
        source: CommandSource::in_session_cli(
            SessionId::new(),
            None,
            PaneId::new(),
            PathBuf::from("/run/koshi/session.sock"),
        ),
        client_id: Some(ClientId::new()),
        issued_at: fixed_time(),
        command: Command::ToggleLockMode(ToggleLockModeArgs::default()),
    };
    let wire = serde_json::to_value(&forged).expect("serialize");
    let err = serde_json::from_value::<CommandEnvelope>(wire).expect_err("rejects");
    assert_eq!(
        err.to_string(),
        "envelope client_id does not match its source"
    );
}

#[test]
fn deserialize_rejects_client_id_mismatch() {
    // The `Internal` source names no client; the wire claims one.
    let forged = CommandEnvelope {
        id: CommandId::new(),
        source: CommandSource::Internal,
        client_id: Some(ClientId::new()),
        issued_at: fixed_time(),
        command: Command::ToggleLockMode(ToggleLockModeArgs::default()),
    };
    let wire = serde_json::to_value(&forged).expect("serialize");
    let err = serde_json::from_value::<CommandEnvelope>(wire).expect_err("rejects");
    assert_eq!(
        err.to_string(),
        "envelope client_id does not match its source"
    );
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
        command: Command::ToggleLockMode(ToggleLockModeArgs::default()),
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
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert_eq!(env.clone().validate(), Ok(env));
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
    let valid = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::KeyBinding {
            client_id: ClientId::new(),
        },
        fixed_time(),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let mut value = serde_json::to_value(&valid).expect("serialize");
    value["client_id"] = serde_json::Value::Null;

    let err = serde_json::from_value::<CommandEnvelope>(value).expect_err("rejects");
    assert_eq!(
        err.to_string(),
        "envelope client_id does not match its source"
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
        emitted_events: vec![Event::Quit, Event::Quit],
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
    // JSON carrying no `target_client` field decodes with it `None`.
    assert_eq!(
        serde_json::from_str::<CommandSource>(r#"{"ExternalCli":{"session_id":null}}"#).unwrap(),
        CommandSource::ExternalCli {
            session_id: None,
            target_client: None,
        }
    );

    let session_id = SessionId::new();
    let uuid = session_id.as_uuid();
    let json = format!(r#"{{"ExternalCli":{{"session_id":"{uuid}"}}}}"#);
    assert_eq!(
        serde_json::from_str::<CommandSource>(&json).unwrap(),
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client: None,
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
    let json = serde_json::to_string(&CommandSource::external_cli(
        Some(session_id),
        Some(client_id),
    ))
    .expect("serialize");

    assert_eq!(
        serde_json::from_str::<OldSource>(&json).expect("deserialize"),
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

    let targeted = CommandSource::external_cli(Some(session_id), Some(client_id));
    assert_eq!(targeted.target_client(), Some(client_id));
    assert_eq!(targeted.client_id(), None);

    assert_eq!(
        CommandSource::external_cli(Some(session_id), None).target_client(),
        None
    );
    assert_eq!(
        CommandSource::in_session_cli(session_id, Some(client_id), pane_id, socket_path)
            .target_client(),
        None
    );
    assert_eq!(
        CommandSource::KeyBinding { client_id }.target_client(),
        None
    );
    assert_eq!(CommandSource::Mouse { client_id }.target_client(), None);
    assert_eq!(
        CommandSource::Plugin {
            plugin_id: PluginId::new(),
        }
        .target_client(),
        None
    );
    assert_eq!(CommandSource::Internal.target_client(), None);

    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::external_cli(Some(session_id), Some(client_id)),
        fixed_time(),
        Command::TogglePaneFullscreen,
    );
    assert_eq!(envelope.client_id, None);
    envelope
        .validate()
        .expect("a source naming a target client is a well-formed envelope");
}

/// The `RunCommandPane` request that spawns `ls` with nothing else chosen.
fn run_ls_args() -> RunCommandPaneArgs {
    RunCommandPaneArgs {
        command: SpawnSpec {
            program: std::path::PathBuf::from("ls"),
            args: vec![],
            cwd: None,
            env: std::collections::BTreeMap::new(),
            shell_kind: crate::process::ShellKind::Other("ls".to_string()),
        },
        cwd: None,
        source: None,
        tab: None,
        direction: Direction::Right,
        stacked: false,
        client: None,
    }
}

#[test]
fn client_id_names_the_issuer_for_key_binding_mouse_and_in_session_cli_only() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let pane_id = PaneId::new();
    let socket_path = PathBuf::from("/run/koshi/session.sock");

    assert_eq!(
        CommandSource::KeyBinding { client_id }.client_id(),
        Some(client_id)
    );
    assert_eq!(
        CommandSource::Mouse { client_id }.client_id(),
        Some(client_id)
    );
    assert_eq!(
        CommandSource::in_session_cli(session_id, Some(client_id), pane_id, socket_path.clone())
            .client_id(),
        Some(client_id)
    );
    assert_eq!(
        CommandSource::in_session_cli(session_id, None, pane_id, socket_path).client_id(),
        None
    );
    assert_eq!(
        CommandSource::external_cli(Some(session_id), Some(client_id)).client_id(),
        None
    );
    assert_eq!(
        CommandSource::Plugin {
            plugin_id: PluginId::new(),
        }
        .client_id(),
        None
    );
    assert_eq!(CommandSource::Internal.client_id(), None);
}

#[test]
fn source_constructors_build_the_matching_variant() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let pane_id = PaneId::new();
    let plugin_id = PluginId::new();
    let socket_path = PathBuf::from("/run/koshi/session.sock");

    assert_eq!(
        CommandSource::key_binding(client_id),
        CommandSource::KeyBinding { client_id }
    );
    assert_eq!(
        CommandSource::mouse(client_id),
        CommandSource::Mouse { client_id }
    );
    assert_eq!(
        CommandSource::in_session_cli(session_id, Some(client_id), pane_id, socket_path.clone()),
        CommandSource::InSessionCli {
            session_id,
            client_id: Some(client_id),
            pane_id,
            socket_path,
        }
    );
    assert_eq!(
        CommandSource::external_cli(Some(session_id), Some(client_id)),
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client: Some(client_id),
        }
    );
    assert_eq!(
        CommandSource::plugin(plugin_id),
        CommandSource::Plugin { plugin_id }
    );
}

#[test]
fn new_derives_the_client_from_every_source() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let pane_id = PaneId::new();
    let socket_path = PathBuf::from("/run/koshi/session.sock");

    let cases: Vec<(CommandSource, Option<ClientId>)> = vec![
        (CommandSource::key_binding(client_id), Some(client_id)),
        (CommandSource::mouse(client_id), Some(client_id)),
        (
            CommandSource::in_session_cli(
                session_id,
                Some(client_id),
                pane_id,
                socket_path.clone(),
            ),
            Some(client_id),
        ),
        (
            CommandSource::in_session_cli(session_id, None, pane_id, socket_path),
            None,
        ),
        (
            CommandSource::external_cli(Some(session_id), Some(client_id)),
            None,
        ),
        (CommandSource::plugin(PluginId::new()), None),
        (CommandSource::Internal, None),
    ];
    assert_eq!(cases.len(), 7);
    for (source, want) in cases {
        let env = CommandEnvelope::new(
            CommandId::new(),
            source.clone(),
            fixed_time(),
            Command::Quit,
        );
        assert_eq!(env.client_id, want, "{source:?}");
        assert_eq!(env.source, source);
        assert_eq!(env.issued_at, fixed_time());
        assert_eq!(env.command, Command::Quit);
    }
}

#[test]
fn validate_returns_an_envelope_whose_client_matches_its_source_unchanged() {
    let client_id = ClientId::new();
    let env = CommandEnvelope {
        id: CommandId::new(),
        source: CommandSource::mouse(client_id),
        client_id: Some(client_id),
        issued_at: fixed_time(),
        command: Command::TogglePaneFullscreen,
    };

    assert_eq!(env.clone().validate(), Ok(env));
}

#[test]
fn validate_rejects_a_missing_client_when_the_source_names_one() {
    let env = CommandEnvelope {
        id: CommandId::new(),
        source: CommandSource::mouse(ClientId::new()),
        client_id: None,
        issued_at: fixed_time(),
        command: Command::TogglePaneFullscreen,
    };

    assert_eq!(env.validate(), Err(CommandEnvelopeError::ClientIdMismatch));
}

#[test]
fn an_envelope_written_without_a_client_id_field_decodes_when_its_source_names_none() {
    let env = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::Internal,
        fixed_time(),
        Command::Quit,
    );
    let mut wire = serde_json::to_value(&env).expect("serialize");
    wire.as_object_mut()
        .expect("an envelope is a JSON object")
        .remove("client_id")
        .expect("the envelope carries a `client_id` field to remove");

    let decoded: CommandEnvelope = serde_json::from_value(wire).expect("deserialize");

    assert_eq!(decoded, env);
}

#[test]
fn an_envelope_written_without_a_client_id_field_is_rejected_when_its_source_names_one() {
    let env = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(ClientId::new()),
        fixed_time(),
        Command::Quit,
    );
    let mut wire = serde_json::to_value(&env).expect("serialize");
    wire.as_object_mut()
        .expect("an envelope is a JSON object")
        .remove("client_id")
        .expect("the envelope carries a `client_id` field to remove");

    let err = serde_json::from_value::<CommandEnvelope>(wire).expect_err("rejects");

    assert_eq!(
        err.to_string(),
        "envelope client_id does not match its source"
    );
}

#[test]
fn an_envelope_written_without_its_command_is_rejected() {
    let env = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::Internal,
        fixed_time(),
        Command::Quit,
    );
    let mut wire = serde_json::to_value(&env).expect("serialize");
    wire.as_object_mut()
        .expect("an envelope is a JSON object")
        .remove("command")
        .expect("the envelope carries a `command` field to remove");

    let err = serde_json::from_value::<CommandEnvelope>(wire).expect_err("rejects");

    assert_eq!(err.to_string(), "missing field `command`");
}

#[test]
fn command_kind_serializes_as_its_variant_name() {
    let kinds = [
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
        CommandKind::Quit,
        CommandKind::Detach,
        CommandKind::DetachAll,
        CommandKind::SwitchSession,
    ];
    assert_eq!(kinds.len(), 20);
    for kind in kinds {
        assert_eq!(
            serde_json::to_value(kind).expect("serialize"),
            json!(variant_name(&kind))
        );
    }
}

#[test]
fn every_payload_free_command_is_a_bare_wire_string() {
    for (command, want) in [
        (Command::ToggleMouseSelect, "\"ToggleMouseSelect\""),
        (Command::TogglePaneFullscreen, "\"TogglePaneFullscreen\""),
        (Command::Quit, "\"Quit\""),
        (Command::DetachAll, "\"DetachAll\""),
    ] {
        assert_eq!(serde_json::to_string(&command).expect("serialize"), want);
        assert_eq!(
            serde_json::from_str::<Command>(want).expect("deserialize"),
            command
        );
    }
}

#[test]
fn plugin_commands_roundtrip_every_variant() {
    let plugin = PluginId::new();
    roundtrip(&PluginCommand::Install(InstallPluginArgs {
        source: "./local/plugin.wasm".to_string(),
    }));
    roundtrip(&PluginCommand::Uninstall(UninstallPluginArgs { plugin }));
    roundtrip(&PluginCommand::Enable(EnablePluginArgs { plugin }));
    roundtrip(&PluginCommand::Disable(DisablePluginArgs { plugin }));
    roundtrip(&PluginCommand::Update(UpdatePluginArgs { plugin }));
    roundtrip(&PluginCommand::Reload(ReloadPluginArgs { plugin }));
}

#[test]
fn focus_targets_and_tab_targets_roundtrip_every_variant() {
    roundtrip(&FocusTarget::Pane(PaneId::new()));
    roundtrip(&FocusTarget::Direction(Direction::Down));
    roundtrip(&TabTarget::Next);
    roundtrip(&TabTarget::Prev);
    roundtrip(&TabTarget::Index(0));
    roundtrip(&TabTarget::Index(usize::MAX));
    roundtrip(&TabTarget::Id(TabId::new()));
}

#[test]
fn selection_kinds_and_copy_targets_roundtrip_every_variant() {
    roundtrip(&SelectionKind::Character);
    roundtrip(&SelectionKind::Word);
    roundtrip(&SelectionKind::Line);
    roundtrip(&SelectionKind::Block);
    roundtrip(&CopyTarget::Osc52);
    roundtrip(&CopyTarget::Native);
}

#[test]
fn extreme_numeric_fields_roundtrip() {
    roundtrip(&ResizePaneArgs {
        pane: None,
        direction: Direction::Left,
        size: i16::MIN,
    });
    roundtrip(&ResizePaneArgs {
        pane: None,
        direction: Direction::Right,
        size: i16::MAX,
    });
    roundtrip(&GridPos {
        row: u64::MAX,
        col: u16::MAX,
    });
    roundtrip(&GridPos { row: 0, col: 0 });
    roundtrip(&MoveTabArgs {
        tab: None,
        index: usize::MAX,
    });
}

#[test]
fn a_resize_size_past_i16_is_rejected() {
    let err = serde_json::from_value::<ResizePaneArgs>(json!({
        "pane": null,
        "direction": "Left",
        "size": 32768
    }))
    .expect_err("rejects");

    assert_eq!(
        err.to_string(),
        "invalid value: integer `32768`, expected i16"
    );
}

#[test]
fn write_to_pane_carries_every_byte_value() {
    let data: Vec<u8> = (0..=255).collect();
    roundtrip(&WriteToPaneArgs {
        pane: Some(PaneId::new()),
        data: data.clone(),
    });

    let value = serde_json::to_value(WriteToPaneArgs { pane: None, data }).expect("serialize");
    assert_eq!(value["data"][0], json!(0));
    assert_eq!(value["data"][255], json!(255));
    assert_eq!(value["data"].as_array().map(Vec::len), Some(256));
}

#[test]
fn args_written_without_their_defaulted_fields_still_decode() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let client_json = serde_json::to_value(client_id).expect("serialize");
    let session_json = serde_json::to_value(session_id).expect("serialize");

    assert_eq!(
        serde_json::from_value::<ClosePaneArgs>(json!({"pane": null, "force": true}))
            .expect("deserialize"),
        ClosePaneArgs {
            pane: None,
            force: true,
            tree: false,
        }
    );
    assert_eq!(
        serde_json::from_value::<CloseTabArgs>(json!({"tab": null, "force": false}))
            .expect("deserialize"),
        CloseTabArgs {
            tab: None,
            force: false,
            tree: false,
        }
    );
    assert_eq!(
        serde_json::from_value::<NewPaneArgs>(json!({
            "source": null,
            "direction": "Right",
            "stacked": false,
            "cwd": null,
            "command": null,
            "client": null
        }))
        .expect("deserialize"),
        new_pane_args()
    );
    assert_eq!(
        serde_json::from_value::<LockModeArgs>(json!({"locked": true})).expect("deserialize"),
        LockModeArgs {
            locked: true,
            client: None,
        }
    );
    assert_eq!(
        serde_json::from_value::<ToggleLockModeArgs>(json!({})).expect("deserialize"),
        ToggleLockModeArgs { client: None }
    );
    assert_eq!(
        serde_json::from_value::<DetachArgs>(json!({})).expect("deserialize"),
        DetachArgs { client: None }
    );
    assert_eq!(
        serde_json::from_value::<SwitchSessionArgs>(json!({"session": session_json}))
            .expect("deserialize"),
        SwitchSessionArgs {
            client: None,
            session: session_id,
        }
    );
    assert_eq!(
        serde_json::from_value::<LockModeArgs>(json!({"locked": false, "client": client_json}))
            .expect("deserialize"),
        LockModeArgs {
            locked: false,
            client: Some(client_id),
        }
    );
}

#[test]
fn run_command_pane_args_written_without_tab_and_client_still_decode() {
    let mut wire = serde_json::to_value(run_ls_args()).expect("serialize");
    let fields = wire.as_object_mut().expect("args are a JSON object");
    fields
        .remove("tab")
        .expect("the args carry a `tab` field to remove");
    fields
        .remove("client")
        .expect("the args carry a `client` field to remove");

    let decoded: RunCommandPaneArgs = serde_json::from_value(wire).expect("deserialize");

    assert_eq!(decoded, run_ls_args());
}

#[test]
fn a_command_with_an_unknown_variant_name_is_rejected() {
    let err = serde_json::from_value::<Command>(json!("Reboot")).expect_err("rejects");

    assert_eq!(
        err.to_string(),
        "unknown variant `Reboot`, expected one of `NewPane`, `ClosePane`, `ResizePane`, `FocusPane`, `NewTab`, `CloseTab`, `FocusTab`, `WriteToPane`, `ToggleLockMode`, `SetLockMode`, `ToggleMouseSelect`, `RunCommandPane`, `Visual`, `Plugin`, `TogglePaneFullscreen`, `MoveTab`, `Quit`, `Detach`, `DetachAll`, `SwitchSession`"
    );
}

#[test]
fn a_command_result_with_an_unknown_variant_name_is_rejected() {
    let err = serde_json::from_value::<CommandResult>(json!({"Pending": {}})).expect_err("rejects");

    assert_eq!(
        err.to_string(),
        "unknown variant `Pending`, expected `Ok` or `Rejected`"
    );
}

#[test]
fn command_envelope_error_implements_std_error() {
    let err: Box<dyn std::error::Error> = Box::new(CommandEnvelopeError::ClientIdMismatch);

    assert_eq!(
        err.to_string(),
        "envelope client_id does not match its source"
    );
}
