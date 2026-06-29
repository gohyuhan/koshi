//! Tests for the command dispatcher skeleton: every `Command` variant routes
//! to a clean rejection, the rejection carries the variant's label, and the
//! result keys back to the originating envelope's command id.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use tile_core::command::{
    ClosePaneArgs, CloseTabArgs, CommandSource, CopyModeCommand, EnablePluginArgs, FocusPaneArgs,
    FocusTabArgs, LockModeArgs, MoveTabArgs, NewPaneArgs, NewTabArgs, PluginCommand,
    RenamePaneArgs, RenameSessionArgs, RenameTabArgs, ResizePaneArgs, RunCommandPaneArgs,
    TabTarget, WriteToPaneArgs,
};
use tile_core::geometry::Direction;
use tile_core::ids::{PaneId, PluginId};
use tile_core::process::{ShellKind, SpawnSpec};
use tile_observability::cleanup::TerminalCleanupGuard;
use tile_pty::backend::state::PtyBackend;
use tile_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{SnapshotProvider, Storage};
use crate::runtime::event::RuntimeEvent;

use super::*;

struct DummySnapshotProvider;
impl SnapshotProvider for DummySnapshotProvider {}

struct DummyStorage;
impl Storage for DummyStorage {}

/// A bare runtime with stub services; dispatch reads none of them while every
/// command rejects. The sender is returned so the inbox stays open.
fn new_runtime() -> (Runtime, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(DummySnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(DummyStorage);
    let (tx, inbox_rx) = mpsc::channel();
    let runtime = Runtime::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        TerminalCleanupGuard::new(),
    );
    (runtime, tx)
}

/// A minimal valid spawn request for the command-carrying variants.
fn spawn_spec() -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/sh"),
        args: Vec::new(),
        cwd: None,
        env: BTreeMap::new(),
        shell_kind: ShellKind::Other("sh".to_string()),
    }
}

/// Wrap a command in an internally-sourced envelope with a fresh id.
fn envelope(command: Command) -> CommandEnvelope {
    CommandEnvelope::new(
        CommandId::new(),
        CommandSource::Internal,
        SystemTime::now(),
        command,
    )
}

#[test]
fn every_command_variant_rejects_with_its_label() {
    let (mut rt, _tx) = new_runtime();

    let cases: Vec<(Command, &str)> = vec![
        (Command::NewPane(NewPaneArgs::default()), "new pane"),
        (Command::ClosePane(ClosePaneArgs::default()), "close pane"),
        (
            Command::ResizePane(ResizePaneArgs {
                pane: None,
                direction: Direction::Left,
                amount: 1,
            }),
            "resize pane",
        ),
        (
            Command::FocusPane(FocusPaneArgs {
                pane: PaneId::new(),
            }),
            "focus pane",
        ),
        (Command::NewTab(NewTabArgs::default()), "new tab"),
        (Command::CloseTab(CloseTabArgs::default()), "close tab"),
        (
            Command::RenameTab(RenameTabArgs {
                tab: None,
                name: "t".to_string(),
            }),
            "rename tab",
        ),
        (
            Command::FocusTab(FocusTabArgs {
                target: TabTarget::Next,
            }),
            "focus tab",
        ),
        (
            Command::WriteToPane(WriteToPaneArgs::default()),
            "write to pane",
        ),
        (Command::ToggleLockMode, "toggle lock mode"),
        (
            Command::SetLockMode(LockModeArgs { locked: true }),
            "set lock mode",
        ),
        (
            Command::RunCommandPane(RunCommandPaneArgs {
                command: spawn_spec(),
                name: None,
                cwd: None,
            }),
            "run command pane",
        ),
        (Command::CopyMode(CopyModeCommand::Enter), "copy mode"),
        (
            Command::Plugin(PluginCommand::Enable(EnablePluginArgs {
                plugin: PluginId::new(),
            })),
            "plugin",
        ),
        (Command::TogglePaneFullscreen, "toggle pane fullscreen"),
        (
            Command::RenamePane(RenamePaneArgs {
                pane: None,
                name: "p".to_string(),
            }),
            "rename pane",
        ),
        (
            Command::MoveTab(MoveTabArgs {
                tab: None,
                index: 0,
            }),
            "move tab",
        ),
        (
            Command::RenameSession(RenameSessionArgs {
                name: "s".to_string(),
            }),
            "rename session",
        ),
    ];

    assert_eq!(cases.len(), 18, "one case per Command variant");

    for (command, label) in cases {
        let env = envelope(command);
        let command_id = env.id;
        let result = rt.dispatch(env);
        assert_eq!(
            result,
            CommandResult::Rejected {
                command_id,
                reason: RejectReason::InvalidState,
                help: Some(format!("{label} not yet implemented")),
            }
        );
    }
}

#[test]
fn rejection_keys_back_to_the_originating_command_id() {
    let (mut rt, _tx) = new_runtime();

    let env = envelope(Command::NewPane(NewPaneArgs::default()));
    let command_id = env.id;

    match rt.dispatch(env) {
        CommandResult::Rejected {
            command_id: rejected_id,
            reason,
            help,
        } => {
            assert_eq!(rejected_id, command_id);
            assert_eq!(reason, RejectReason::InvalidState);
            assert_eq!(help.as_deref(), Some("new pane not yet implemented"));
        }
        CommandResult::Ok { .. } => panic!("skeleton dispatch must reject, never apply"),
    }
}
