//! Tests for command dispatch: validation rejects ill-formed commands before
//! the match, and a command that passes validation but has no handler yet
//! routes to a clean labelled rejection.
//!
//! Every case runs against an empty runtime (no sessions). That bounds what a
//! "real validation path" can reach here: `Unauthorized`, `SourceClientStale`,
//! `TargetNotFound`, and `TargetGone` (via the apply-time re-check) all fire
//! without any populated state. `InvalidState` (session-admission) needs a
//! session in a `Stopping`/`Stopped` state, for which no construction API
//! exists yet, so it is not exercised here.

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
use tile_core::ids::{ClientId, PaneId, PluginId, TabId};
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

/// A bare runtime with stub services and no sessions. The sender is returned so
/// the inbox stays open.
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

/// Wrap a command in an envelope from the given source with a fresh id.
fn envelope_from(source: CommandSource, command: Command) -> CommandEnvelope {
    CommandEnvelope::new(CommandId::new(), source, SystemTime::now(), command)
}

/// Wrap a command in an internally-sourced envelope with a fresh id.
fn envelope(command: Command) -> CommandEnvelope {
    envelope_from(CommandSource::Internal, command)
}

#[test]
fn passing_validation_reaches_the_unimplemented_reject() {
    let (mut rt, _tx) = new_runtime();

    // Commands that pass validation from an internal source: they name no pane
    // target (or only an optional one left unset), so they fall through to the
    // not-yet-implemented arm of the match.
    let cases: Vec<(Command, &str)> = vec![
        (Command::NewPane(NewPaneArgs::default()), "new pane"),
        (Command::NewTab(NewTabArgs::default()), "new tab"),
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
        (
            Command::RenameSession(RenameSessionArgs {
                name: "s".to_string(),
            }),
            "rename session",
        ),
    ];

    for (command, label) in cases {
        let env = envelope(command);
        let command_id = env.id;
        assert_eq!(
            rt.dispatch(env),
            CommandResult::Rejected {
                command_id,
                reason: RejectReason::InvalidState,
                help: Some(format!("{label} not yet implemented")),
            }
        );
    }
}

#[test]
fn client_scoped_command_without_a_client_is_unauthorized() {
    let (mut rt, _tx) = new_runtime();

    let commands = vec![
        Command::FocusPane(FocusPaneArgs {
            pane: PaneId::new(),
        }),
        Command::ToggleLockMode,
        Command::SetLockMode(LockModeArgs { locked: true }),
        Command::TogglePaneFullscreen,
    ];

    for command in commands {
        let env = envelope(command);
        let command_id = env.id;
        assert_eq!(
            rt.dispatch(env),
            CommandResult::Rejected {
                command_id,
                reason: RejectReason::Unauthorized,
                help: Some("command requires an attached client".to_string()),
            }
        );
    }
}

#[test]
fn client_source_with_no_attached_client_is_stale() {
    let (mut rt, _tx) = new_runtime();

    // A keybinding names a client, but no session holds it on an empty runtime.
    let source = CommandSource::key_binding(ClientId::new());
    let env = envelope_from(source, Command::ClosePane(ClosePaneArgs::default()));
    let command_id = env.id;

    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: None,
        }
    );
}

#[test]
fn explicit_pane_target_absent_is_not_found() {
    let (mut rt, _tx) = new_runtime();

    let env = envelope(Command::ClosePane(ClosePaneArgs {
        pane: Some(PaneId::new()),
    }));
    let command_id = env.id;

    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
}

#[test]
fn default_pane_target_without_context_is_not_found() {
    let (mut rt, _tx) = new_runtime();

    // No explicit pane and an internal source: nothing to default to.
    let env = envelope(Command::ClosePane(ClosePaneArgs::default()));
    let command_id = env.id;

    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("no target and no focused pane to default to".to_string()),
        }
    );
}

#[test]
fn write_to_pane_routes_the_pane_target() {
    let (mut rt, _tx) = new_runtime();

    let env = envelope(Command::WriteToPane(WriteToPaneArgs {
        pane: Some(PaneId::new()),
        data: vec![b'x'],
    }));
    let command_id = env.id;

    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
}

#[test]
fn rename_pane_default_target_without_context_is_not_found() {
    let (mut rt, _tx) = new_runtime();

    let env = envelope(Command::RenamePane(RenamePaneArgs {
        pane: None,
        name: "p".to_string(),
    }));
    let command_id = env.id;

    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("no target and no focused pane to default to".to_string()),
        }
    );
}

#[test]
fn resize_pane_default_target_without_context_is_not_found() {
    let (mut rt, _tx) = new_runtime();

    let env = envelope(Command::ResizePane(ResizePaneArgs {
        pane: None,
        direction: tile_core::geometry::Direction::Left,
        amount: 1,
    }));
    let command_id = env.id;

    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("no target and no focused pane to default to".to_string()),
        }
    );
}

#[test]
fn tab_command_without_session_context_is_not_found() {
    let (mut rt, _tx) = new_runtime();

    // An internal source has no session context to resolve a tab within.
    let commands = vec![
        Command::CloseTab(CloseTabArgs {
            tab: Some(TabId::new()),
        }),
        Command::CloseTab(CloseTabArgs::default()),
        Command::RenameTab(RenameTabArgs {
            tab: None,
            name: "t".to_string(),
        }),
        Command::MoveTab(MoveTabArgs {
            tab: None,
            index: 0,
        }),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Next,
        }),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(TabId::new()),
        }),
    ];

    for command in commands {
        let env = envelope(command);
        let command_id = env.id;
        assert_eq!(
            rt.dispatch(env),
            CommandResult::Rejected {
                command_id,
                reason: RejectReason::TargetNotFound,
                help: Some("no session context for tab target".to_string()),
            }
        );
    }
}

#[test]
fn recheck_reports_gone_when_pane_is_absent() {
    let (rt, _tx) = new_runtime();

    match rt.recheck_pane_present(PaneId::new()) {
        Err(rejection) => assert_eq!(rejection.reason, RejectReason::TargetGone),
        Ok(()) => panic!("an absent pane must re-check as gone"),
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
        CommandResult::Ok { .. } => panic!("dispatch must reject, never apply"),
    }
}
