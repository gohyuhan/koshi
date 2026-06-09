//! Tests for the canonical command vocabulary.

use super::*;
use crate::ids::PaneId;

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
}

#[test]
fn pane_commands_roundtrip() {
    roundtrip(&Command::NewPane(NewPaneArgs {
        direction: Some(Direction::Right),
        name: Some("logs".to_string()),
        ..NewPaneArgs::default()
    }));
    roundtrip(&Command::ClosePane(ClosePaneArgs {
        pane: Some(PaneId::new()),
    }));
    roundtrip(&Command::ResizePane(ResizePaneArgs {
        pane: None,
        direction: Direction::Up,
        amount: 4,
    }));
    roundtrip(&Command::FocusPane(FocusPaneArgs {
        pane: PaneId::new(),
    }));
    roundtrip(&Command::RenamePane(RenamePaneArgs {
        pane: None,
        name: "editor".to_string(),
    }));
}

#[test]
fn tab_and_session_commands_roundtrip() {
    roundtrip(&Command::FocusTab(FocusTabArgs {
        target: TabTarget::Next,
    }));
    roundtrip(&Command::FocusTab(FocusTabArgs {
        target: TabTarget::Index(2),
    }));
    roundtrip(&Command::MoveTab(MoveTabArgs {
        tab: None,
        index: 0,
    }));
    roundtrip(&Command::RenameSession(RenameSessionArgs {
        name: "work".to_string(),
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
fn copy_mode_commands_roundtrip() {
    roundtrip(&Command::CopyMode(CopyModeCommand::Enter));
    roundtrip(&Command::CopyMode(CopyModeCommand::MoveCursor(
        MoveCursorArgs {
            unit: MoveUnit::Word,
            direction: Direction::Left,
        },
    )));
    roundtrip(&Command::CopyMode(CopyModeCommand::SetSelection(
        SetSelectionArgs {
            kind: SelectionKind::Block,
            anchor: GridPos { row: 10, col: 0 },
            cursor: GridPos { row: 12, col: 40 },
        },
    )));
    roundtrip(&Command::CopyMode(CopyModeCommand::Copy(CopyArgs {
        target: CopyTarget::Osc52,
    })));
    roundtrip(&Command::CopyMode(CopyModeCommand::Search(SearchArgs {
        query: "error".to_string(),
        regex: true,
        case_sensitive: false,
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
    repr.split('(').next().unwrap_or(&repr).to_string()
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
                amount: 1,
            }),
            "ResizePane",
        ),
        (
            Command::FocusPane(FocusPaneArgs {
                pane: PaneId::new(),
            }),
            "FocusPane",
        ),
        (Command::NewTab(NewTabArgs::default()), "NewTab"),
        (Command::CloseTab(CloseTabArgs::default()), "CloseTab"),
        (
            Command::RenameTab(RenameTabArgs {
                tab: None,
                name: "t".to_string(),
            }),
            "RenameTab",
        ),
        (
            Command::FocusTab(FocusTabArgs {
                target: TabTarget::Next,
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
                name: None,
                cwd: None,
            }),
            "RunCommandPane",
        ),
        (Command::CopyMode(CopyModeCommand::Enter), "CopyMode"),
        (
            Command::Plugin(PluginCommand::Reload(ReloadPluginArgs {
                plugin: PluginId::new(),
            })),
            "Plugin",
        ),
        (Command::TogglePaneFullscreen, "TogglePaneFullscreen"),
        (
            Command::RenamePane(RenamePaneArgs {
                pane: None,
                name: "p".to_string(),
            }),
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
            Command::RenameSession(RenameSessionArgs {
                name: "s".to_string(),
            }),
            "RenameSession",
        ),
    ];
    assert_eq!(cases.len(), 18);
    for (value, name) in &cases {
        assert_eq!(&variant_name(value), name);
    }
}

#[test]
fn copy_mode_variant_names_are_canonical() {
    let cases: Vec<(CopyModeCommand, &str)> = vec![
        (CopyModeCommand::Enter, "Enter"),
        (CopyModeCommand::Exit, "Exit"),
        (
            CopyModeCommand::MoveCursor(MoveCursorArgs {
                unit: MoveUnit::Cell,
                direction: Direction::Down,
            }),
            "MoveCursor",
        ),
        (
            CopyModeCommand::SetSelection(SetSelectionArgs {
                kind: SelectionKind::Character,
                anchor: GridPos { row: 0, col: 0 },
                cursor: GridPos { row: 0, col: 1 },
            }),
            "SetSelection",
        ),
        (CopyModeCommand::ClearSelection, "ClearSelection"),
        (
            CopyModeCommand::Copy(CopyArgs {
                target: CopyTarget::Osc52,
            }),
            "Copy",
        ),
        (
            CopyModeCommand::Search(SearchArgs {
                query: "q".to_string(),
                regex: false,
                case_sensitive: false,
            }),
            "Search",
        ),
        (CopyModeCommand::SearchNext, "SearchNext"),
        (CopyModeCommand::SearchPrev, "SearchPrev"),
    ];
    assert_eq!(cases.len(), 9);
    for (value, name) in &cases {
        assert_eq!(&variant_name(value), name);
    }
}
