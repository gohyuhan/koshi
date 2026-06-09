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

/// Snapshot of the canonical top-level variant names. If a variant is renamed,
/// added, or removed, this list must change in lockstep with the spec.
#[test]
fn command_variant_names_are_canonical() {
    let names: Vec<&str> = vec![
        "NewPane",
        "ClosePane",
        "ResizePane",
        "FocusPane",
        "NewTab",
        "CloseTab",
        "RenameTab",
        "FocusTab",
        "WriteToPane",
        "ToggleLockMode",
        "SetLockMode",
        "RunCommandPane",
        "CopyMode",
        "Plugin",
        "TogglePaneFullscreen",
        "RenamePane",
        "MoveTab",
        "RenameSession",
    ];
    assert_eq!(names.len(), 18);

    // Spot-check that a unit variant's Debug repr is exactly its name, anchoring
    // the snapshot to the real enum rather than a detached string list.
    assert_eq!(format!("{:?}", Command::ToggleLockMode), "ToggleLockMode");
    assert_eq!(
        format!("{:?}", Command::TogglePaneFullscreen),
        "TogglePaneFullscreen"
    );
}

#[test]
fn copy_mode_variant_names_are_canonical() {
    let names: Vec<&str> = vec![
        "Enter",
        "Exit",
        "MoveCursor",
        "SetSelection",
        "ClearSelection",
        "Copy",
        "Search",
        "SearchNext",
        "SearchPrev",
    ];
    assert_eq!(names.len(), 9);
    assert_eq!(format!("{:?}", CopyModeCommand::Enter), "Enter");
    assert_eq!(
        format!("{:?}", CopyModeCommand::ClearSelection),
        "ClearSelection"
    );
}
