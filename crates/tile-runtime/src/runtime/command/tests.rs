//! Tests for command dispatch: validation rejects ill-formed commands before
//! the match, and a command that passes validation but has no handler yet
//! routes to a clean labelled rejection.
//!
//! Rejection cases (no context) run against an empty runtime. Cases that need
//! populated state — explicit/default/focused target resolution, in-session-CLI
//! pane defaulting, and `InvalidState` session admission — build sessions with
//! the helpers below and install them into the runtime's `sessions` map.

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
use tile_core::geometry::Size;
use tile_core::ids::{ClientId, PaneId, PluginId, SessionId, TabId};
use tile_core::process::{ShellKind, SpawnSpec};
use tile_observability::cleanup::TerminalCleanupGuard;
use tile_pane::pane::state::PaneRecord;
use tile_pty::backend::state::PtyBackend;
use tile_session::client::{Client, ClientRegistry};
use tile_session::session::state::{Session, Tab};
use tile_session::session::tab_ops;
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

/// A `Starting` session with the given id and no tabs, clients, or panes.
fn bare_session(id: SessionId) -> Session {
    Session::new(id, "s".to_string(), ClientRegistry::new())
}

/// A `Stopping` session: a tab takes it `Starting` -> `Running`, then a stop is
/// requested.
fn stopping_session(id: SessionId) -> Session {
    let mut session = bare_session(id);
    let _ = tab_ops::new_tab(&mut session, "t".to_string(), SystemTime::now());
    session.request_stop();
    session
}

/// Register a fresh `Spawning` pane in the session's registry.
fn add_pane(session: &mut Session, pane: PaneId) {
    session
        .panes
        .insert(PaneRecord::new(pane, SystemTime::now()))
        .expect("unique pane id");
}

/// Add a tab whose single-leaf layout is `root_pane`.
fn add_tab(session: &mut Session, tab_id: TabId, root_pane: PaneId) {
    let index = session.tabs.len();
    session
        .tabs
        .insert(tab_id, Tab::new(tab_id, "t".to_string(), index, root_pane));
}

/// Attach a client viewing `tab`, optionally with `focused` recorded there.
fn add_client(session: &mut Session, client_id: ClientId, tab: TabId, focused: Option<PaneId>) {
    let mut client = Client::new(
        client_id,
        session.id,
        SystemTime::now(),
        Size { cols: 80, rows: 24 },
        tab,
    );
    if let Some(pane) = focused {
        client.update_focused_pane(tab, pane);
    }
    session.attach_client(client);
}

#[test]
fn passing_validation_reaches_the_unimplemented_reject() {
    let (mut rt, _tx) = new_runtime();

    // From an internal source (no session, no client) a store-level plugin
    // command needs no session/client/pane context, so it passes validation and
    // falls through to the not-yet-implemented arm of the match.
    let env = envelope(Command::Plugin(PluginCommand::Enable(EnablePluginArgs {
        plugin: PluginId::new(),
    })));
    let command_id = env.id;

    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("plugin not yet implemented".to_string()),
        }
    );
}

#[test]
fn client_scoped_command_without_a_client_is_unauthorized() {
    let (mut rt, _tx) = new_runtime();

    let commands = vec![
        Command::FocusPane(FocusPaneArgs {
            pane: PaneId::new(),
        }),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Next,
        }),
        Command::ToggleLockMode,
        Command::SetLockMode(LockModeArgs { locked: true }),
        Command::TogglePaneFullscreen,
        Command::CopyMode(CopyModeCommand::Enter),
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
            help: Some("no session context".to_string()),
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
            help: Some("no session context".to_string()),
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
            help: Some("no session context".to_string()),
        }
    );
}

#[test]
fn tab_command_without_session_context_is_not_found() {
    let (mut rt, _tx) = new_runtime();

    // An internal source has no session context to resolve a tab within.
    // (FocusTab is client-scoped and rejects earlier as Unauthorized, so it is
    // covered by the client-scoped test, not here.)
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
    ];

    for command in commands {
        let env = envelope(command);
        let command_id = env.id;
        assert_eq!(
            rt.dispatch(env),
            CommandResult::Rejected {
                command_id,
                reason: RejectReason::TargetNotFound,
                help: Some("no session context".to_string()),
            }
        );
    }
}

#[test]
fn session_scoped_command_without_session_is_not_found() {
    let (mut rt, _tx) = new_runtime();

    // These create or rename within a session; an internal source resolves to
    // no session, so there is nothing to act on.
    let commands = vec![
        Command::NewTab(NewTabArgs::default()),
        Command::RenameSession(RenameSessionArgs {
            name: "s".to_string(),
        }),
        Command::RunCommandPane(RunCommandPaneArgs {
            command: spawn_spec(),
            name: None,
            cwd: None,
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
                help: Some("no session context".to_string()),
            }
        );
    }
}

#[test]
fn new_pane_explicit_source_absent_is_not_found() {
    let (mut rt, _tx) = new_runtime();

    let env = envelope(Command::NewPane(NewPaneArgs {
        source: Some(PaneId::new()),
        ..NewPaneArgs::default()
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
fn new_pane_without_an_anchor_is_not_found() {
    let (mut rt, _tx) = new_runtime();

    // Every new-pane shape splits a source leaf (`source: None` = the focused
    // pane); an internal source has no focused pane to anchor on, in any mode.
    let cases = vec![
        NewPaneArgs::default(),
        NewPaneArgs {
            direction: Some(tile_core::geometry::Direction::Right),
            ..NewPaneArgs::default()
        },
        NewPaneArgs {
            stacked: true,
            ..NewPaneArgs::default()
        },
        NewPaneArgs {
            in_place: true,
            ..NewPaneArgs::default()
        },
    ];

    for args in cases {
        let env = envelope(Command::NewPane(args));
        let command_id = env.id;
        assert_eq!(
            rt.dispatch(env),
            CommandResult::Rejected {
                command_id,
                reason: RejectReason::TargetNotFound,
                help: Some("no session context".to_string()),
            }
        );
    }
}

#[test]
fn new_pane_defaults_to_the_focused_pane() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, Some(pane));
    rt.sessions.insert(session.id, session);

    // No explicit source: the focused pane anchors the split, so it resolves.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("new pane not yet implemented".to_string()),
        }
    );
}

#[test]
fn rejection_keys_back_to_the_originating_command_id() {
    let (mut rt, _tx) = new_runtime();

    // A store-level plugin command passes validation from an internal source,
    // so it reaches the match and the reject keys back to its command id.
    let env = envelope(Command::Plugin(PluginCommand::Enable(EnablePluginArgs {
        plugin: PluginId::new(),
    })));
    let command_id = env.id;

    match rt.dispatch(env) {
        CommandResult::Rejected {
            command_id: rejected_id,
            reason,
            help,
        } => {
            assert_eq!(rejected_id, command_id);
            assert_eq!(reason, RejectReason::InvalidState);
            assert_eq!(help.as_deref(), Some("plugin not yet implemented"));
        }
        CommandResult::Ok { .. } => panic!("dispatch must reject, never apply"),
    }
}

#[test]
fn explicit_pane_in_live_session_passes_to_unimplemented() {
    let (mut rt, _tx) = new_runtime();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    rt.sessions.insert(session.id, session);

    let env = envelope(Command::ClosePane(ClosePaneArgs { pane: Some(pane) }));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("close pane not yet implemented".to_string()),
        }
    );
}

#[test]
fn explicit_pane_in_stopping_session_is_invalid_state() {
    let (mut rt, _tx) = new_runtime();
    let pane = PaneId::new();
    let mut session = stopping_session(SessionId::new());
    add_pane(&mut session, pane);
    rt.sessions.insert(session.id, session);

    // An internal source has no acting session, so admission is reached only
    // via the pane's owning session — which is stopping.
    let env = envelope(Command::ClosePane(ClosePaneArgs { pane: Some(pane) }));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("session is stopping".to_string()),
        }
    );
}

#[test]
fn in_session_cli_defaults_to_its_source_pane() {
    let (mut rt, _tx) = new_runtime();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let pane = PaneId::new();
    let mut session = bare_session(session_id);
    add_pane(&mut session, pane);
    add_client(&mut session, client_id, TabId::new(), None);
    rt.sessions.insert(session.id, session);

    // No explicit pane: an in-session CLI targets the pane it was issued from.
    let source = CommandSource::in_session_cli(session_id, client_id, pane, PathBuf::from("/sock"));
    let env = envelope_from(source, Command::ClosePane(ClosePaneArgs::default()));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("close pane not yet implemented".to_string()),
        }
    );
}

#[test]
fn in_session_cli_with_missing_source_pane_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let mut session = bare_session(session_id);
    add_client(&mut session, client_id, TabId::new(), None);
    rt.sessions.insert(session.id, session);

    // The source pane has since closed; the stale source resolves to nothing.
    let source =
        CommandSource::in_session_cli(session_id, client_id, PaneId::new(), PathBuf::from("/sock"));
    let env = envelope_from(source, Command::ClosePane(ClosePaneArgs::default()));
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
fn client_focused_pane_resolves_to_unimplemented() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, Some(pane));
    rt.sessions.insert(session.id, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ToggleLockMode,
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("toggle lock mode not yet implemented".to_string()),
        }
    );
}

#[test]
fn client_without_focused_pane_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let mut session = bare_session(SessionId::new());
    add_client(&mut session, client_id, TabId::new(), None);
    rt.sessions.insert(session.id, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ToggleLockMode,
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("no focused pane".to_string()),
        }
    );
}

#[test]
fn focused_pane_that_no_longer_exists_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    // Focus records a pane that was never registered (or has since been removed):
    // resolution checks the registry, not just that a focus is recorded.
    let mut session = bare_session(SessionId::new());
    add_client(&mut session, client_id, TabId::new(), Some(PaneId::new()));
    rt.sessions.insert(session.id, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ToggleLockMode,
    );
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
fn copy_mode_resolves_the_focused_pane() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, Some(pane));
    rt.sessions.insert(session.id, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::CopyMode(CopyModeCommand::Enter),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("copy mode not yet implemented".to_string()),
        }
    );
}

#[test]
fn focus_pane_in_the_active_tab_resolves() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab_id, pane);
    add_client(&mut session, client_id, tab_id, Some(pane));
    rt.sessions.insert(session.id, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs { pane }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("focus pane not yet implemented".to_string()),
        }
    );
}

#[test]
fn focus_pane_outside_the_active_tab_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let outside = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    // `outside` exists in the registry but is not in the active tab's layout,
    // proving the check is tab-scoped, not mere global existence.
    add_pane(&mut session, outside);
    add_tab(&mut session, tab_id, pane);
    add_client(&mut session, client_id, tab_id, Some(pane));
    rt.sessions.insert(session.id, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs { pane: outside }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("pane not in the client's active tab".to_string()),
        }
    );
}

#[test]
fn in_session_cli_source_pane_in_another_session_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let session_a = SessionId::new();
    let pane_in_b = PaneId::new();

    let mut a = bare_session(session_a);
    add_client(&mut a, client_id, TabId::new(), None);
    rt.sessions.insert(a.id, a);

    // The pane lives in a *different* session; scoped resolution must reject it.
    let mut b = bare_session(SessionId::new());
    add_pane(&mut b, pane_in_b);
    rt.sessions.insert(b.id, b);

    let source =
        CommandSource::in_session_cli(session_a, client_id, pane_in_b, PathBuf::from("/sock"));
    let env = envelope_from(source, Command::ClosePane(ClosePaneArgs::default()));
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
fn external_cli_default_pane_without_a_client_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let session_id = SessionId::new();
    rt.sessions.insert(session_id, bare_session(session_id));

    // A session resolves, but an external CLI names no client to default from.
    let source = CommandSource::external_cli(Some(session_id));
    let env = envelope_from(source, Command::ClosePane(ClosePaneArgs::default()));
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
fn focused_default_outside_active_tab_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let in_tab = PaneId::new();
    let focused_elsewhere = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, in_tab);
    // `focused_elsewhere` is in the registry but not in the active tab's layout.
    add_pane(&mut session, focused_elsewhere);
    add_tab(&mut session, tab, in_tab);
    add_client(&mut session, client_id, tab, Some(focused_elsewhere));
    rt.sessions.insert(session.id, session);

    // ToggleLockMode defaults through the focused pane; it is outside the tab.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ToggleLockMode,
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("pane not in the client's active tab".to_string()),
        }
    );
}

#[test]
fn run_command_pane_requires_a_pane_anchor() {
    let (mut rt, _tx) = new_runtime();
    let session_id = SessionId::new();
    rt.sessions.insert(session_id, bare_session(session_id));

    // A session alone is not enough — RunCommandPane spawns into a split and
    // needs a pane anchor, like NewPane.
    let source = CommandSource::external_cli(Some(session_id));
    let env = envelope_from(
        source,
        Command::RunCommandPane(RunCommandPaneArgs {
            command: spawn_spec(),
            name: None,
            cwd: None,
        }),
    );
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
fn in_session_cli_session_id_is_authoritative_over_a_mismatched_client() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let claimed_session = SessionId::new();

    // The envelope claims session A, but the client is attached to session B.
    // Session A is looked up by its id and rejects the unattached client rather
    // than silently acting on B.
    rt.sessions
        .insert(claimed_session, bare_session(claimed_session));
    let mut b = bare_session(SessionId::new());
    add_client(&mut b, client_id, TabId::new(), None);
    rt.sessions.insert(b.id, b);

    let source = CommandSource::in_session_cli(
        claimed_session,
        client_id,
        PaneId::new(),
        PathBuf::from("/sock"),
    );
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
fn focus_tab_relative_resolves_from_a_live_active_tab() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, None);
    rt.sessions.insert(session.id, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Next,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("focus tab not yet implemented".to_string()),
        }
    );
}

#[test]
fn focus_tab_relative_with_a_stale_active_tab_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let live_tab = TabId::new();
    let stale_tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    // The session has a real tab, but the client's active tab points elsewhere.
    add_tab(&mut session, live_tab, pane);
    add_client(&mut session, client_id, stale_tab, None);
    rt.sessions.insert(session.id, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Prev,
        }),
    );
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
fn focus_target_with_removed_registry_record_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    // Pane is in the tab's layout but NOT in the registry (record removed).
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, Some(pane));
    rt.sessions.insert(session.id, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs { pane }),
    );
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
fn in_session_cli_tab_default_uses_the_source_pane_tab() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(session_id);
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    // Client's active tab is B, but the CLI command was issued from tab A's pane.
    add_client(&mut session, client_id, tab_b, None);
    rt.sessions.insert(session.id, session);

    // CloseTab with no explicit tab — InSessionCli should resolve via the tab
    // containing pane_a (tab A), not the client's active tab (tab B).
    let source =
        CommandSource::in_session_cli(session_id, client_id, pane_a, PathBuf::from("/sock"));
    let env = envelope_from(source, Command::CloseTab(CloseTabArgs::default()));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("close tab not yet implemented".to_string()),
        }
    );
}

#[test]
fn in_session_cli_tab_default_with_removed_source_pane_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(session_id);
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, None);
    rt.sessions.insert(session.id, session);

    // The source pane_id doesn't exist in the registry (nor any tab layout).
    let stale_pane = PaneId::new();
    let source =
        CommandSource::in_session_cli(session_id, client_id, stale_pane, PathBuf::from("/sock"));
    let env = envelope_from(source, Command::CloseTab(CloseTabArgs::default()));
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
