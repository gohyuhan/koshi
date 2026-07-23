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
use std::sync::{mpsc, Arc, Barrier};
use std::time::{Duration, Instant, SystemTime};

use koshi_core::command::{
    ClosePaneArgs, CloseTabArgs, CommandSource, CopyArgs, CopyTarget, EnablePluginArgs,
    FocusPaneArgs, FocusTabArgs, GridPos, LockModeArgs, MoveTabArgs, NewPaneArgs, NewTabArgs,
    PluginCommand, RenamePaneArgs, RenameSessionArgs, RenameTabArgs, ResizePaneArgs,
    RunCommandPaneArgs, Selection, SelectionKind, TabTarget, VisualCommand, WriteToPaneArgs,
};
use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::geometry::{Direction, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, PluginId, SessionId, TabId};
use koshi_core::naming;
use koshi_core::process::{ExitStatus, PtySize, ShellKind, SpawnSpec};
use koshi_layout::edit::split_leaf;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::MIN_PANE_SIZE;
use koshi_layout::tree::{LayoutChild, SplitNode};
use koshi_pane::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use koshi_pane::pane::policy::PaneExitPolicy;
use koshi_pane::pane::state::{PaneKind, PaneRecord};
use koshi_pty::backend::state::{PtyBackend, PtyHandle};
use koshi_pty::error::PtyError;
use koshi_session::client::{pane_viewport, Client, ClientRegistry};
use koshi_session::session::pane_ops::NewPaneSpec;
use koshi_session::session::state::{Session, Tab};
use koshi_session::session::tab_ops;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use crate::runtime::event::RuntimeEvent;

use super::*;

/// A bare runtime with stub services and no sessions. The sender is returned so
/// the inbox stays open.
fn new_runtime() -> (Server, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel();
    let runtime = Server::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx.clone(),
        Direction::Right,
    );
    (runtime, tx)
}

/// Like [`new_runtime`], but also hands back the concrete fake backend so a test
/// can drive spawn failures and assert on spawned panes, specs, and resizes.
/// Both the runtime and the returned handle share one backend.
fn new_runtime_with_fake() -> (Server, Arc<FakePtyBackend>, mpsc::Sender<RuntimeEvent>) {
    let fake = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = fake.clone();
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel();
    let runtime = Server::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx.clone(),
        Direction::Right,
    );
    (runtime, fake, tx)
}

/// The id of the single pane in `session` that is not `source` — the freshly
/// split pane. Panics unless exactly one other pane exists.
fn other_pane(rt: &Server, session: SessionId, source: PaneId) -> PaneId {
    let mut others = rt.sessions[&session]
        .panes
        .list()
        .map(PaneRecord::id)
        .filter(|id| *id != source);
    let pane = others.next().expect("a second pane exists");
    assert!(others.next().is_none(), "exactly one other pane");
    pane
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
    Session::new(
        id,
        "s".to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    )
}

/// A `Stopping` session: a tab takes it `Starting` -> `Running`, then a stop is
/// requested.
fn stopping_session(id: SessionId) -> Session {
    let mut session = bare_session(id);
    let _ = tab_ops::commit_new_tab(
        &mut session,
        TabId::new(),
        PaneId::new(),
        "t".to_string(),
        None,
        NewPaneSpec::default(),
        SystemTime::now(),
    );
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

/// Poll the fake backend until `pane` records a kill, then return the history.
/// The close handler kills on a detached thread, so the recorded policy is
/// awaited rather than read immediately; panics if none arrives within 5s.
fn wait_for_kill(fake: &FakePtyBackend, pane: PaneId) -> Vec<KillPolicy> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let kills = fake.kills(pane).expect("pane spawned in the fake");
        if !kills.is_empty() {
            return kills;
        }
        assert!(Instant::now() < deadline, "no kill arrived within 5s");
        thread::sleep(Duration::from_millis(1));
    }
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
fn commands_needing_a_session_are_not_found_without_one() {
    let (mut rt, _tx) = new_runtime();

    // Each of these resolves a client inside the acting session, so with no
    // session there is nothing to resolve.
    let commands = vec![
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
        Command::SetLockMode(LockModeArgs {
            locked: true,
            client: None,
        }),
        Command::TogglePaneFullscreen,
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
fn selection_from_a_clientless_source_is_stale() {
    let (mut rt, _tx) = new_runtime();

    // A highlight belongs to the client that made it, so a source naming no
    // client has no highlight to touch — never another client's.
    let env = envelope(Command::Visual(VisualCommand::ClearSelection(
        ClearSelectionArgs {
            pane: PaneId::new(),
        },
    )));
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
        force: false,
        tree: false,
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
fn write_to_a_running_pane_delivers_the_bytes() {
    let (mut rt, fake, _tx, _sid, _client_id, _root, pane_a, _size_a) = resize_fixture();

    // An explicit target on a live pane injects the bytes into its child and
    // completes with no events — the write is a side effect, not a state change.
    let env = envelope(Command::WriteToPane(WriteToPaneArgs {
        pane: Some(pane_a),
        data: vec![b'l', b's', b'\n'],
    }));
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert!(emitted_events.is_empty());
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(fake.writes(pane_a).unwrap(), vec![vec![b'l', b's', b'\n']]);
}

/// A commanded write has NO visibility guard, unlike a typed key: the caller
/// named this pane on purpose, and a pane the layout has no room to draw still
/// holds a live shell. Shrinking the terminal must not silently swallow a
/// scripted `koshi input --pane <id> "…"`.
///
/// This pins the asymmetry deliberately: `Server::typed_pane` refuses an
/// undrawn pane because a person cannot type at what they cannot see, and this
/// path must not inherit that rule.
#[test]
fn write_to_a_suppressed_pane_still_reaches_its_shell() {
    let (mut rt, fake, _tx, _sid, client_id, _root, pane_a, _size_a) = resize_fixture();

    // Shrink the client's terminal until the tab has no room to draw its panes.
    rt.handle_client_resize(client_id, Size { cols: 3, rows: 3 });
    assert!(
        rt.build_snapshot(client_id)
            .expect("snapshot")
            .session
            .active_tab
            .all_suppressed,
        "test setup: the panes must be suppressed at this size"
    );

    let env = envelope(Command::WriteToPane(WriteToPaneArgs {
        pane: Some(pane_a),
        data: vec![b'l', b's'],
    }));
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(fake.writes(pane_a).unwrap(), vec![vec![b'l', b's']]);
}

/// The client's scroll offset for the pane — `0` follows live output.
fn client_scroll_offset(rt: &Server, client: ClientId, pane: PaneId) -> usize {
    rt.sessions()
        .values()
        .next()
        .unwrap()
        .clients
        .get(client)
        .unwrap()
        .scroll_offset(pane)
}

/// A client-sourced write snaps that client's scrolled-up view back to live
/// output, exactly as typing the same bytes into the pane would. The bytes are
/// documented as arriving "as if typed there," so the view follows to the
/// prompt. An `Internal`-sourced write (no client) has no view to move.
#[test]
fn a_client_sourced_write_to_pane_snaps_that_client_view_to_live_output() {
    let (mut rt, _fake, _tx, _sid, client_id, _root, pane_a, _size_a) = resize_fixture();
    rt.handle_pty_output(pane_a, &b"\n".repeat(200)); // push lines into history
    rt.scroll_up(client_id, pane_a, 3);
    assert_eq!(client_scroll_offset(&rt, client_id, pane_a), 3);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane: Some(pane_a),
            data: vec![b'l', b's', b'\n'],
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(client_scroll_offset(&rt, client_id, pane_a), 0);
}

/// A client-sourced write also drops that client's highlight in the target
/// pane, the way typing over a selection does. Without the clear the leftover
/// highlight would hold the view and the child's next output would re-anchor it
/// away from live, so the snap could not stick.
#[test]
fn a_client_sourced_write_clears_the_clients_highlight_in_the_pane() {
    let (mut rt, _fake, _tx, _sid, client_id, _root, pane_a, _size_a) = resize_fixture();
    rt.handle_pty_output(pane_a, &b"\n".repeat(200));
    rt.scroll_up(client_id, pane_a, 3);
    rt.client_mut(client_id)
        .unwrap()
        .set_selection(pane_a, a_selection());

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane: Some(pane_a),
            data: vec![b'l', b's', b'\n'],
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert_eq!(client_scroll_offset(&rt, client_id, pane_a), 0);
    let client = rt
        .sessions()
        .values()
        .next()
        .unwrap()
        .clients
        .get(client_id)
        .unwrap();
    assert_eq!(client.selection(pane_a), None);
}

/// An empty payload sends no bytes to the child, so it is not input: it leaves a
/// parked scrollback view exactly where it was.
#[test]
fn an_empty_client_sourced_write_leaves_a_parked_view_alone() {
    let (mut rt, _fake, _tx, _sid, client_id, _root, pane_a, _size_a) = resize_fixture();
    rt.handle_pty_output(pane_a, &b"\n".repeat(200));
    rt.scroll_up(client_id, pane_a, 3);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane: Some(pane_a),
            data: Vec::new(),
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(client_scroll_offset(&rt, client_id, pane_a), 3);
}

/// A plugin pane has no PTY, so there is nowhere for the bytes to land: the
/// write is rejected rather than aimed at a child that does not exist. The
/// pane's id still has a live PTY handle in the fake backend, so only its KIND
/// can explain the rejection.
#[test]
fn write_to_a_plugin_pane_is_rejected_and_writes_nothing() {
    let (mut rt, fake, _tx, sid, _client_id, _root, pane_a, _size_a) = resize_fixture();

    // Re-file `pane_a`'s record under `Plugin`, keeping its id and its place in
    // the layout.
    let session = rt.sessions.get_mut(&sid).expect("session");
    let created_at = session.panes.get(pane_a).expect("pane record").created_at;
    session.panes.remove(pane_a);
    session
        .panes
        .insert(PaneRecord::new_with_kind(
            pane_a,
            PaneKind::Plugin {
                plugin_id: PluginId::new(),
            },
            created_at,
        ))
        .expect("re-inserting a removed pane id");

    let env = envelope(Command::WriteToPane(WriteToPaneArgs {
        pane: Some(pane_a),
        data: vec![b'l', b's'],
    }));
    let command_id = env.id;

    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane is not a terminal pane".to_string()),
        }
    );
    assert_eq!(fake.writes(pane_a).unwrap(), Vec::<Vec<u8>>::new());
}

#[test]
fn write_to_pane_defaults_to_the_clients_focused_pane() {
    let (mut rt, fake, _tx, _sid, client_id, _root, pane_a, _size_a) = resize_fixture();

    // No explicit target: a keybinding source writes to the client's focused
    // pane, which the split left on `pane_a`.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane: None,
            data: vec![b'a'],
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(fake.writes(pane_a).unwrap(), vec![vec![b'a']]);
}

#[test]
fn write_to_pane_via_in_session_cli_defaults_to_the_issuing_pane() {
    let (mut rt, fake, _tx, sid, client_id, _root, pane_a, _size_a) = resize_fixture();

    // Issued from inside `pane_a` with no explicit target: the captured issuing
    // pane is the target.
    let source =
        CommandSource::in_session_cli(sid, Some(client_id), pane_a, PathBuf::from("/sock"));
    let env = envelope_from(
        source,
        Command::WriteToPane(WriteToPaneArgs {
            pane: None,
            data: vec![b'b'],
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(fake.writes(pane_a).unwrap(), vec![vec![b'b']]);
}

#[test]
fn write_to_an_exited_pane_is_rejected() {
    let (mut rt, fake, _tx, sid, _client_id, _root, pane_a, _size_a) = resize_fixture();

    // Drive the live split to `Exited`; a dead pane takes no input.
    rt.sessions
        .get_mut(&sid)
        .unwrap()
        .panes
        .get_mut(pane_a)
        .unwrap()
        .update_lifecycle(PaneLifecycleEvent::ProcessExited {
            code: Some(0),
            at: SystemTime::now(),
        })
        .unwrap();

    let env = envelope(Command::WriteToPane(WriteToPaneArgs {
        pane: Some(pane_a),
        data: vec![b'x'],
    }));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane is not accepting input".to_string()),
        }
    );
    assert!(fake.writes(pane_a).unwrap().is_empty());
}

#[test]
fn write_to_a_closing_pane_is_rejected() {
    let (mut rt, fake, _tx, sid, _client_id, _root, pane_a, _size_a) = resize_fixture();

    // A pane mid-teardown (`Closing`) takes no input.
    rt.sessions
        .get_mut(&sid)
        .unwrap()
        .panes
        .get_mut(pane_a)
        .unwrap()
        .update_lifecycle(PaneLifecycleEvent::CloseRequested {
            since: SystemTime::now(),
        })
        .unwrap();

    let env = envelope(Command::WriteToPane(WriteToPaneArgs {
        pane: Some(pane_a),
        data: vec![b'x'],
    }));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane is not accepting input".to_string()),
        }
    );
    assert!(fake.writes(pane_a).unwrap().is_empty());
}

#[test]
fn write_from_a_plugin_source_is_denied() {
    let (mut rt, fake, _tx, _sid, _client_id, _root, pane_a, _size_a) = resize_fixture();

    // A plugin write needs the `pane_write` capability, not yet grantable, so it
    // is denied before any byte reaches the pane.
    let env = envelope_from(
        CommandSource::plugin(PluginId::new()),
        Command::WriteToPane(WriteToPaneArgs {
            pane: Some(pane_a),
            data: vec![b'x'],
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::Unauthorized,
            help: Some("plugin lacks the pane_write capability".to_string()),
        }
    );
    assert!(fake.writes(pane_a).unwrap().is_empty());
}

#[test]
fn write_backend_failure_is_reported() {
    // A pane `Running` in the model but absent from the backend (its child died
    // between the liveness check and the write) makes the backend write fail;
    // the failure is reported, not swallowed.
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, Some(pane));
    session
        .panes
        .get_mut(pane)
        .unwrap()
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .unwrap();
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope(Command::WriteToPane(WriteToPaneArgs {
        pane: Some(pane),
        data: vec![b'x'],
    }));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane is not accepting input".to_string()),
        }
    );
}

#[test]
fn write_with_empty_data_is_a_noop_ok() {
    let (mut rt, _fake, _tx, _sid, _client_id, _root, pane_a, _size_a) = resize_fixture();

    // An empty payload is a legal no-op write: it applies with no events.
    let env = envelope(Command::WriteToPane(WriteToPaneArgs {
        pane: Some(pane_a),
        data: Vec::new(),
    }));
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert!(emitted_events.is_empty());
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn rename_pane_default_target_without_context_is_not_found() {
    let (mut rt, _tx) = new_runtime();

    let env = envelope(Command::RenamePane(RenamePaneArgs { pane: None }));
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
        direction: koshi_core::geometry::Direction::Left,
        size: 1,
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
    let commands = vec![
        Command::CloseTab(CloseTabArgs {
            tab: Some(TabId::new()),
            force: false,
            tree: false,
        }),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Next,
            client: None,
        }),
        Command::CloseTab(CloseTabArgs::default()),
        Command::RenameTab(RenameTabArgs { tab: None }),
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
        Command::RunCommandPane(RunCommandPaneArgs {
            command: spawn_spec(),
            cwd: None,
            source: None,
            tab: None,
            direction: None,
            stacked: false,
            client: None,
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

    // A new-pane anchors on a source leaf (`source: None` = the focused pane);
    // an internal source has no focused pane to anchor on. The stacked shape
    // resolves its anchor the same way, so it rejects identically.
    let cases = vec![
        NewPaneArgs::default(),
        NewPaneArgs {
            direction: Some(koshi_core::geometry::Direction::Right),
            ..NewPaneArgs::default()
        },
        NewPaneArgs {
            stacked: true,
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
    let session_id = session.id;
    rt.sessions.insert(session_id, session);

    // No explicit source: the focused pane anchors the split, and the new pane
    // auto-focuses, spawns its PTY, and is resized into the new geometry —
    // PaneCreated + LayoutChanged + PaneFocused + PtyResized(new pane). The root
    // has no PTY, so it contributes no PtyResized.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    // The split registered a second pane in the tab.
    assert_eq!(rt.sessions[&session_id].panes.len(), 2);
}

#[test]
fn new_pane_without_direction_splits_in_the_runtime_default() {
    // A runtime seeded with a Down default; a directionless new-pane must
    // split downward, not rightward.
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel();
    let mut rt = Server::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx.clone(),
        Direction::Down,
    );

    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, Some(pane));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let before = rt.sessions[&sid].tabs[&tab].layout().clone();
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    match rt.dispatch(env) {
        CommandResult::Ok { .. } => {}
        other => panic!("expected Ok, got {other:?}"),
    }

    let new_pane = other_pane(&rt, sid, pane);
    let expected =
        split_leaf(&before, pane, new_pane, Direction::Down).expect("split on the source leaf");
    assert_eq!(rt.sessions[&sid].tabs[&tab].layout(), &expected);
}

#[test]
fn new_pane_stacked_on_a_plain_leaf_creates_a_stack() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, Some(pane));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // A stacked new-pane on a plain leaf turns the leaf into a two-member
    // stack: the source collapses to a header, the new pane is the expanded
    // active member and takes focus — PaneCreated + LayoutChanged +
    // PaneFocused + PtyResized(new pane).
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            stacked: true,
            ..NewPaneArgs::default()
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let new_pane = other_pane(&rt, sid, pane);
    assert_eq!(
        rt.sessions[&sid].tabs[&tab].layout(),
        &LayoutNode::Split(SplitNode::stack(vec![pane, new_pane], 1))
    );
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .focused_pane(tab),
        Some(new_pane)
    );
    assert!(rt.pty_handles.contains_key(&new_pane));
}

#[test]
fn new_pane_stacked_onto_a_stack_member_appends() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, a);
    add_pane(&mut session, b);
    add_tab(&mut session, tab, a);
    session
        .tabs
        .get_mut(&tab)
        .unwrap()
        .update_layout(LayoutNode::Split(SplitNode::stack(vec![a, b], 1)));
    add_client(&mut session, client_id, tab, Some(b));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Stacking from a pane already inside a stack appends to that stack: the
    // new pane joins as the last member, becomes the expanded active one, and
    // every earlier member collapses.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            stacked: true,
            ..NewPaneArgs::default()
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let new_pane = {
        let mut others = rt.sessions[&sid]
            .panes
            .list()
            .map(PaneRecord::id)
            .filter(|id| *id != a && *id != b);
        let pane = others.next().expect("a third pane exists");
        assert!(others.next().is_none(), "exactly one new pane");
        pane
    };
    assert_eq!(
        rt.sessions[&sid].tabs[&tab].layout(),
        &LayoutNode::Split(SplitNode::stack(vec![a, b, new_pane], 2))
    );
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .focused_pane(tab),
        Some(new_pane)
    );
}

#[test]
fn new_pane_stacked_ignores_direction() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, Some(pane));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // `--stacked` with a direction still stacks — a stack has no direction, so
    // the flag routes to the stack edit and the direction is never read.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            stacked: true,
            direction: Some(koshi_core::geometry::Direction::Down),
            ..NewPaneArgs::default()
        }),
    );
    match rt.dispatch(env) {
        CommandResult::Ok { .. } => {}
        other => panic!("expected Ok, got {other:?}"),
    }

    let new_pane = other_pane(&rt, sid, pane);
    assert_eq!(
        rt.sessions[&sid].tabs[&tab].layout(),
        &LayoutNode::Split(SplitNode::stack(vec![pane, new_pane], 1))
    );
}

#[test]
fn new_pane_stacked_with_a_missing_source_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, Some(pane));
    rt.sessions.insert(session.id, session);

    // A stacked request naming a pane that does not exist resolves its target
    // like any other new-pane and rejects `TargetNotFound`.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            stacked: true,
            source: Some(PaneId::new()),
            ..NewPaneArgs::default()
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
fn new_pane_stacked_with_no_space_is_min_size() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    // A 2x1 viewport cannot hold a stack: a two-member stack needs one header
    // row plus the active member's minimum rows.
    let mut client = Client::new(
        client_id,
        session.id,
        SystemTime::now(),
        Size { cols: 2, rows: 1 },
        tab,
    );
    client.update_focused_pane(tab, pane);
    session.attach_client(client);
    rt.sessions.insert(session.id, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            stacked: true,
            ..NewPaneArgs::default()
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("not enough space for a new pane".to_string()),
        }
    );
}

#[test]
fn new_pane_stacked_spawn_failure_leaves_no_trace() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    fake.fail_spawns_with(PtyError::Spawn {
        detail: "boom".to_string(),
    });
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);
    let before_layout = rt.sessions[&sid].tabs[&tab].layout().clone();

    // Launch-then-commit holds for the stacked shape too: the child cannot
    // launch, so no stack is created and nothing is committed.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            stacked: true,
            ..NewPaneArgs::default()
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("failed to launch the pane's process".to_string()),
        }
    );
    assert_eq!(rt.sessions[&sid].panes.len(), 1);
    assert_eq!(rt.sessions[&sid].tabs[&tab].layout(), &before_layout);
    assert!(rt.pty_handles.is_empty());
    assert!(fake.spawned_panes().is_empty());
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .focused_pane(tab),
        Some(root)
    );
}

#[test]
fn new_pane_with_no_space_is_min_size() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    // A 2x1 viewport cannot hold a split at minimum size.
    let mut client = Client::new(
        client_id,
        session.id,
        SystemTime::now(),
        Size { cols: 2, rows: 1 },
        tab,
    );
    client.update_focused_pane(tab, pane);
    session.attach_client(client);
    rt.sessions.insert(session.id, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("not enough space for a new pane".to_string()),
        }
    );
}

#[test]
fn new_pane_explicit_pane_in_session_without_clients_is_rejected() {
    let (mut rt, _tx) = new_runtime();

    // Session A holds the acting client.
    let client_id = ClientId::new();
    let id_a = SessionId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session_a = bare_session(id_a);
    add_pane(&mut session_a, pane_a);
    add_tab(&mut session_a, tab_a, pane_a);
    add_client(&mut session_a, client_id, tab_a, Some(pane_a));
    rt.sessions.insert(id_a, session_a);

    // Session B owns the explicit `--pane` target and has no client at all, so
    // nothing can view the new pane's tab.
    let id_b = SessionId::new();
    let tab_b = TabId::new();
    let pane_b = PaneId::new();
    let mut session_b = bare_session(id_b);
    add_pane(&mut session_b, pane_b);
    add_tab(&mut session_b, tab_b, pane_b);
    rt.sessions.insert(id_b, session_b);

    // A global `--pane` targets B, but B has no client to adopt onto the tab, so
    // the pane could never be sized or shown: reject rather than strand it.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_b),
            ..NewPaneArgs::default()
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("no attached client to view the new pane's tab".to_string()),
        }
    );
    // Rejected before mutating: neither session grew a pane.
    assert_eq!(rt.sessions[&id_b].panes.len(), 1);
    assert_eq!(rt.sessions[&id_a].panes.len(), 1);
}

#[test]
fn new_pane_with_stale_focus_outside_active_tab_is_rejected() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let leaf = PaneId::new();
    let ghost = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, leaf);
    // `ghost` is registered but never placed in the tab's layout.
    add_pane(&mut session, ghost);
    add_tab(&mut session, tab, leaf);
    // The client's active-tab focus points at `ghost` — a stale entry that is
    // not a leaf of the active tab.
    add_client(&mut session, client_id, tab, Some(ghost));
    rt.sessions.insert(session.id, session);

    // The default source is the stale focus; it must reject, never split a tab.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
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
fn close_pane_registered_but_in_no_tab_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    let session_id = session.id;
    rt.sessions.insert(session_id, session);

    // The pane exists in the registry but no tab's layout holds it: validation
    // (registry membership) passes, and the handler's own tab lookup rejects
    // before anything mutates.
    let env = envelope(Command::ClosePane(ClosePaneArgs {
        pane: Some(pane),
        force: false,
        tree: false,
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
    assert_eq!(rt.sessions[&session_id].panes.len(), 1);
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
    let env = envelope(Command::ClosePane(ClosePaneArgs {
        pane: Some(pane),
        force: false,
        tree: false,
    }));
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
fn in_session_cli_close_defaults_to_its_source_pane() {
    let (mut rt, _tx) = new_runtime();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(session_id);
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    rt.sessions.insert(session_id, session);

    // Grow a second pane; the split focuses it and parks its handle.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, session_id, root);

    // No explicit pane: an in-session CLI closes the pane it was issued from.
    // The captured pane is the split one, so the root survives and inherits
    // focus — PaneClosing + PaneRemoved + LayoutChanged + PaneFocused.
    let source = CommandSource::in_session_cli(
        session_id,
        Some(client_id),
        new_pane,
        PathBuf::from("/sock"),
    );
    let env = envelope_from(source, Command::ClosePane(ClosePaneArgs::default()));
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(rt.sessions[&session_id].panes.get(new_pane).is_none());
    assert_eq!(
        rt.sessions[&session_id].tabs[&tab].layout(),
        &LayoutNode::Pane(root)
    );
    assert_eq!(
        rt.sessions[&session_id]
            .clients
            .get(client_id)
            .unwrap()
            .focused_pane(tab),
        Some(root)
    );
    assert!(!rt.pty_handles.contains_key(&new_pane));
}

#[test]
fn in_session_cli_with_missing_source_pane_is_gone() {
    let (mut rt, _tx) = new_runtime();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let mut session = bare_session(session_id);
    add_client(&mut session, client_id, TabId::new(), None);
    rt.sessions.insert(session.id, session);

    // The source pane has since closed; the command issued from it is refused
    // before any target resolution.
    let source = CommandSource::in_session_cli(
        session_id,
        Some(client_id),
        PaneId::new(),
        PathBuf::from("/sock"),
    );
    let env = envelope_from(source, Command::ClosePane(ClosePaneArgs::default()));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetGone,
            help: Some("source pane no longer exists".to_string()),
        }
    );
}

/// A single-client session focused on one pane, returned with the session id
/// so a lock test can dispatch and read the client's mode back.
fn lock_fixture() -> (Server, mpsc::Sender<RuntimeEvent>, ClientId, SessionId) {
    let (mut rt, tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, Some(pane));
    let sid = session.id;
    rt.sessions.insert(sid, session);
    (rt, tx, client_id, sid)
}

/// Read `client_id`'s lock mode out of session `sid`.
fn lock_mode_of(rt: &Server, sid: SessionId, client_id: ClientId) -> LockMode {
    rt.sessions[&sid]
        .clients
        .get(client_id)
        .expect("client")
        .lock_mode()
}

#[test]
fn toggle_lock_mode_locks_an_unlocked_client() {
    let (mut rt, _tx, client_id, sid) = lock_fixture();

    // A default-Normal client toggles into Locked: exactly one event.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(lock_mode_of(&rt, sid, client_id), LockMode::Locked);
}

#[test]
fn toggle_mouse_select_flips_the_client_flag() {
    let (mut rt, _tx, client_id, sid) = lock_fixture();
    let grabs = |rt: &Server| {
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .expect("client")
            .mouse_select()
    };
    assert!(!grabs(&rt), "a fresh client does not grab the mouse");

    // First toggle turns mouse-select on; the command carries no bus event.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ToggleMouseSelect,
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert!(emitted_events.is_empty());
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert!(grabs(&rt), "the toggle turned mouse-select on");

    // A second toggle turns it back off.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ToggleMouseSelect,
    );
    let _ = rt.dispatch(env);
    assert!(!grabs(&rt), "the second toggle turned mouse-select off");
}

#[test]
fn toggle_lock_mode_unlocks_a_locked_client() {
    let (mut rt, _tx, client_id, sid) = lock_fixture();
    rt.sessions
        .get_mut(&sid)
        .expect("session")
        .clients
        .get_mut(client_id)
        .expect("client")
        .update_lock_mode(LockMode::Locked);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(lock_mode_of(&rt, sid, client_id), LockMode::Normal);
}

#[test]
fn set_lock_mode_locks_then_unlocks() {
    let (mut rt, _tx, client_id, sid) = lock_fixture();

    let lock = envelope_from(
        CommandSource::key_binding(client_id),
        Command::SetLockMode(LockModeArgs {
            locked: true,
            client: None,
        }),
    );
    let lock_id = lock.id;
    match rt.dispatch(lock) {
        CommandResult::Ok {
            command_id,
            emitted_events,
        } => {
            assert_eq!(command_id, lock_id);
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(lock_mode_of(&rt, sid, client_id), LockMode::Locked);

    let unlock = envelope_from(
        CommandSource::key_binding(client_id),
        Command::SetLockMode(LockModeArgs {
            locked: false,
            client: None,
        }),
    );
    let unlock_id = unlock.id;
    match rt.dispatch(unlock) {
        CommandResult::Ok {
            command_id,
            emitted_events,
        } => {
            assert_eq!(command_id, unlock_id);
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(lock_mode_of(&rt, sid, client_id), LockMode::Normal);
}

#[test]
fn setting_the_current_lock_mode_emits_nothing() {
    let (mut rt, _tx, client_id, sid) = lock_fixture();

    // The client is already Normal; unlocking it changes nothing.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::SetLockMode(LockModeArgs {
            locked: false,
            client: None,
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 0);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(lock_mode_of(&rt, sid, client_id), LockMode::Normal);
}

#[test]
fn lock_mode_is_isolated_between_clients() {
    let (mut rt, _tx) = new_runtime();
    let (alice, bob) = (ClientId::new(), ClientId::new());
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    // Both clients view the same tab and pane, proving lock is per-client, not
    // per-pane.
    add_client(&mut session, alice, tab, Some(pane));
    add_client(&mut session, bob, tab, Some(pane));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(alice),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let _ = rt.dispatch(env);

    assert_eq!(lock_mode_of(&rt, sid, alice), LockMode::Locked);
    assert_eq!(lock_mode_of(&rt, sid, bob), LockMode::Normal);
}

#[test]
fn lock_mode_toggles_without_a_focused_pane() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let mut session = bare_session(SessionId::new());
    // No pane, no focus: lock is client-scoped, so it still applies.
    add_client(&mut session, client_id, TabId::new(), None);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(lock_mode_of(&rt, sid, client_id), LockMode::Locked);
}

#[test]
fn client_without_focused_pane_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let mut session = bare_session(SessionId::new());
    add_client(&mut session, client_id, TabId::new(), None);
    rt.sessions.insert(session.id, session);

    // Fullscreen acts on the focused pane; this client has none.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::TogglePaneFullscreen,
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
        Command::TogglePaneFullscreen,
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
fn a_highlight_command_names_its_own_pane_and_ignores_the_focused_one() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let focused = PaneId::new();
    let other = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, focused);
    add_pane(&mut session, other);
    add_tab(&mut session, tab, focused);
    add_client(&mut session, client_id, tab, Some(focused));
    rt.sessions.insert(session.id, session);

    // Highlight the pane that is NOT focused: the command names it, so the
    // focused pane is not consulted and never falls in as a default.
    let selection = a_selection();
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::Visual(VisualCommand::SetSelection(SetSelectionArgs {
            pane: other,
            selection,
        })),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    let client = rt.client_mut(client_id).expect("client");
    assert_eq!(
        client.selection(other),
        Some(selection),
        "the named pane is highlighted"
    );
    assert_eq!(
        client.selection(focused),
        None,
        "the focused pane is untouched"
    );
}

#[test]
fn a_highlight_command_for_a_pane_that_is_gone_is_rejected() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, Some(pane));
    rt.sessions.insert(session.id, session);

    // A drag that raced the pane closing names a pane the session no longer has.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::Visual(VisualCommand::SetSelection(SetSelectionArgs {
            pane: PaneId::new(),
            selection: a_selection(),
        })),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetGone,
            help: None,
        }
    );
}

#[test]
fn clearing_a_pane_with_no_highlight_is_accepted_and_changes_nothing() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, Some(pane));
    rt.sessions.insert(session.id, session);

    // The ways a highlight ends fire without first checking one was up, so
    // clearing nothing must not be an error.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::Visual(VisualCommand::ClearSelection(ClearSelectionArgs { pane })),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(
        rt.client_mut(client_id).expect("client").selection(pane),
        None
    );
}

#[test]
fn a_host_paste_lands_whole_in_the_focused_pane() {
    // The OS paste key pressed in the outer terminal: the text arrives as one
    // block and is written whole — a pasted Tab lands in the shell instead of
    // firing the tab-switch binding.
    let (mut rt, fake, _tx, _sid, client_id, _root, pane_a, _size) = resize_fixture();

    rt.handle_host_paste(client_id, "ls\ttmp\ncat");
    let writes = fake.writes(pane_a).expect("pane writes");
    assert_eq!(
        writes.last().expect("one write"),
        b"ls\ttmp\rcat",
        "raw bytes, line break as the Enter byte"
    );
}

#[test]
fn a_host_paste_wraps_in_bracketed_markers_when_the_pane_turned_them_on() {
    let (mut rt, fake, _tx, _sid, client_id, _root, pane_a, _size) = resize_fixture();
    rt.handle_pty_output(pane_a, b"\x1b[?2004h");

    rt.handle_host_paste(client_id, "ok");
    let writes = fake.writes(pane_a).expect("pane writes");
    assert_eq!(writes.last().expect("one write"), b"\x1b[200~ok\x1b[201~");
}

#[test]
fn a_host_paste_clears_the_highlight_in_the_pasted_pane() {
    let (mut rt, _fake, _tx, _sid, client_id, _root, pane_a, _size) = resize_fixture();
    let client = rt.client_mut(client_id).expect("client");
    client.set_selection(pane_a, a_selection());

    rt.handle_host_paste(client_id, "x");
    assert_eq!(
        rt.client_mut(client_id).expect("client").selection(pane_a),
        None,
        "pasted text reached the child, so the highlight is gone"
    );
}

#[test]
fn the_copy_command_surface_rejects_the_interactive_copy_is_the_release() {
    // `VisualCommand::Copy` is the future IPC/plugin surface and is unbuilt;
    // a person copies by releasing the selection, which needs no command.
    let (mut rt, _fake, _tx, _sid, client_id, _root, _pane_a, _size) = resize_fixture();

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::Visual(VisualCommand::Copy(CopyArgs {
            target: CopyTarget::Osc52,
        })),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("copy not yet implemented".to_string()),
        }
    );
}

/// A one-cell character highlight, for tests that care which pane a command
/// lands on rather than what it highlights.
fn a_selection() -> Selection {
    Selection {
        kind: SelectionKind::Character,
        anchor: GridPos { row: 0, col: 0 },
        cursor: GridPos { row: 0, col: 1 },
    }
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

    // The pane is already this client's focus, so the command resolves and
    // completes as a no-op: applied, zero events.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(pane),
            client: None,
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 0);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn focus_pane_by_direction_with_no_neighbor_is_target_not_found() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab_id, pane);
    add_client(&mut session, client_id, tab_id, Some(pane));
    rt.sessions.insert(session.id, session);

    // A fully valid session context whose sole pane has no left neighbor:
    // the geometric lookup itself reports the miss.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Direction(Direction::Left),
            client: None,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("no pane in that direction".to_string()),
        }
    );
}

#[test]
fn quit_marks_immediate_teardown_and_the_loop_flag() {
    let (mut rt, _tx) = new_runtime();

    let env = envelope(Command::Quit);
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
    assert!(rt.quit_requested());
    assert!(rt.immediate_shutdown);
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
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(outside),
            client: None,
        }),
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
fn focus_from_a_sessionless_source_has_no_session_context() {
    let (mut rt, _tx) = new_runtime();

    // An internal source names neither client nor session; the resolver has
    // no session to find a target client in.
    let env = envelope(Command::FocusPane(FocusPaneArgs {
        target: FocusTarget::Pane(PaneId::new()),
        client: None,
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
fn focus_pane_moves_focus_records_mru_and_emits_one_event() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, a);
    add_pane(&mut session, b);
    add_tab(&mut session, tab_id, a);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutChild::new(LayoutNode::Pane(a)),
                LayoutChild::new(LayoutNode::Pane(b)),
            ],
        )));
    add_client(&mut session, client_id, tab_id, Some(a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(b),
            client: None,
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // Exactly the focus fact: a plain move changes no layout and no PTY.
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &rt.sessions[&sid];
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("client")
            .focused_pane(tab_id),
        Some(b)
    );
    assert_eq!(session.tabs[&tab_id].focus_mru().first(), Some(&b));
}

#[test]
fn focus_suppressed_pane_is_rejected_and_mutates_nothing() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, a);
    add_pane(&mut session, b);
    add_tab(&mut session, tab_id, a);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Vertical,
            vec![
                LayoutChild::new(LayoutNode::Pane(a)),
                LayoutChild::new(LayoutNode::Pane(b)),
            ],
        )));
    // A 2x1 viewport is below every pane's border-inclusive floor, so the
    // solve suppresses the whole split — `b` cannot take focus.
    let mut client = Client::new(
        client_id,
        session.id,
        SystemTime::now(),
        Size { cols: 2, rows: 1 },
        tab_id,
    );
    client.update_focused_pane(tab_id, a);
    session.attach_client(client);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(b),
            client: None,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane is suppressed; not enough space to show it".to_string()),
        }
    );
    // Nothing moved: focus and MRU are untouched.
    let session = &rt.sessions[&sid];
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("client")
            .focused_pane(tab_id),
        Some(a)
    );
    assert!(!session.tabs[&tab_id].focus_mru().contains(&b));
}

#[test]
fn focus_collapsed_stack_member_activates_the_stack() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, a);
    add_pane(&mut session, b);
    add_tab(&mut session, tab_id, a);
    // `a` is the expanded member; `b` is collapsed to a header strip.
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::stack(vec![a, b], 0)));
    add_client(&mut session, client_id, tab_id, Some(a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(b),
            client: None,
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // LayoutChanged (the stack swapped members) + PaneFocused. No
            // PtyResized: neither pane has a live PTY here.
            assert_eq!(emitted_events.len(), 2);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &rt.sessions[&sid];
    assert_eq!(
        session.tabs[&tab_id].layout(),
        &LayoutNode::Split(SplitNode::stack(vec![a, b], 1))
    );
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("client")
            .focused_pane(tab_id),
        Some(b)
    );
    assert_eq!(session.tabs[&tab_id].focus_mru().first(), Some(&b));
}

#[test]
fn focus_active_stack_member_changes_no_layout() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, a);
    add_pane(&mut session, b);
    add_pane(&mut session, c);
    add_tab(&mut session, tab_id, a);
    // `b` is the stack's expanded member: focusing it needs no activation.
    let layout = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutChild::new(LayoutNode::Pane(a)),
            LayoutChild::new(LayoutNode::Split(SplitNode::stack(vec![b, c], 0))),
        ],
    ));
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(layout.clone());
    add_client(&mut session, client_id, tab_id, Some(a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(b),
            client: None,
        }),
    );
    match rt.dispatch(env) {
        CommandResult::Ok { emitted_events, .. } => {
            // Only the focus fact — the tree is untouched.
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &rt.sessions[&sid];
    assert_eq!(session.tabs[&tab_id].layout(), &layout);
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("client")
            .focused_pane(tab_id),
        Some(b)
    );
}

#[test]
fn focus_already_focused_collapsed_member_reactivates_without_a_focus_event() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, a);
    add_pane(&mut session, b);
    add_tab(&mut session, tab_id, a);
    // The client's focus already points at `b`, but `b` sits collapsed (as
    // happens when another actor swaps the stack's active member).
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::stack(vec![a, b], 0)));
    add_client(&mut session, client_id, tab_id, Some(b));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(b),
            client: None,
        }),
    );
    match rt.dispatch(env) {
        CommandResult::Ok { emitted_events, .. } => {
            // LayoutChanged only: the stack expands `b`, but the focus did not
            // move, so no PaneFocused is emitted.
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &rt.sessions[&sid];
    assert_eq!(
        session.tabs[&tab_id].layout(),
        &LayoutNode::Split(SplitNode::stack(vec![a, b], 1))
    );
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("client")
            .focused_pane(tab_id),
        Some(b)
    );
}

#[test]
fn focus_explicit_client_wins_over_the_issuer() {
    let (mut rt, _tx) = new_runtime();
    let issuer = ClientId::new();
    let target = ClientId::new();
    let tab_x = TabId::new();
    let tab_y = TabId::new();
    let (p, q, r) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, p);
    add_pane(&mut session, q);
    add_pane(&mut session, r);
    add_tab(&mut session, tab_x, p);
    add_tab(&mut session, tab_y, q);
    session
        .tabs
        .get_mut(&tab_y)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutChild::new(LayoutNode::Pane(q)),
                LayoutChild::new(LayoutNode::Pane(r)),
            ],
        )));
    add_client(&mut session, issuer, tab_x, Some(p));
    add_client(&mut session, target, tab_y, Some(q));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // `r` is not in the issuer's active tab — the pane resolves against the
    // NAMED client's active tab, proving the explicit target wins.
    let env = envelope_from(
        CommandSource::key_binding(issuer),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(r),
            client: Some(target),
        }),
    );
    match rt.dispatch(env) {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &rt.sessions[&sid];
    assert_eq!(
        session
            .clients
            .get(target)
            .expect("target")
            .focused_pane(tab_y),
        Some(r)
    );
    // The issuer's own focus is untouched.
    assert_eq!(
        session
            .clients
            .get(issuer)
            .expect("issuer")
            .focused_pane(tab_x),
        Some(p)
    );
}

#[test]
fn focus_unattached_explicit_client_is_rejected_without_fallback() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab_id, pane);
    add_client(&mut session, client_id, tab_id, None);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // The named client does not exist; the valid issuer is NOT used instead.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(pane),
            client: Some(ClientId::new()),
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    let session = &rt.sessions[&sid];
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("client")
            .focused_pane(tab_id),
        None
    );
}

#[test]
fn focus_from_a_clientless_source_defaults_to_the_sole_client() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, a);
    add_pane(&mut session, b);
    add_tab(&mut session, tab_id, a);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutChild::new(LayoutNode::Pane(a)),
                LayoutChild::new(LayoutNode::Pane(b)),
            ],
        )));
    add_client(&mut session, client_id, tab_id, Some(a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::ExternalCli {
            session_id: Some(sid),
        },
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(b),
            client: None,
        }),
    );
    match rt.dispatch(env) {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .expect("client")
            .focused_pane(tab_id),
        Some(b)
    );
}

#[test]
fn focus_from_a_clientless_source_with_two_clients_is_ambiguous() {
    let (mut rt, _tx) = new_runtime();
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab_id, pane);
    add_client(&mut session, ClientId::new(), tab_id, None);
    add_client(&mut session, ClientId::new(), tab_id, None);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::ExternalCli {
            session_id: Some(sid),
        },
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(pane),
            client: None,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some("several clients are attached; name the target client".to_string()),
        }
    );
}

#[test]
fn focus_with_no_attached_client_at_all_is_stale() {
    let (mut rt, _tx) = new_runtime();
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab_id, pane);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::ExternalCli {
            session_id: Some(sid),
        },
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(pane),
            client: None,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: Some("no client is attached to the session".to_string()),
        }
    );
}

#[test]
fn focus_an_exited_pane_succeeds() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, a);
    add_pane(&mut session, b);
    add_tab(&mut session, tab_id, a);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutChild::new(LayoutNode::Pane(a)),
                LayoutChild::new(LayoutNode::Pane(b)),
            ],
        )));
    // A dead pane is a visible, focusable placeholder until it is removed.
    let exited_at = SystemTime::now();
    {
        let record = session.panes.get_mut(b).expect("record");
        let _ = record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);
        let _ = record.update_lifecycle(PaneLifecycleEvent::ProcessExited {
            code: Some(0),
            at: exited_at,
        });
        assert_eq!(
            *record.lifecycle(),
            PaneLifecycle::Exited {
                code: Some(0),
                at: exited_at,
            }
        );
    }
    add_client(&mut session, client_id, tab_id, Some(a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(b),
            client: None,
        }),
    );
    match rt.dispatch(env) {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .expect("client")
            .focused_pane(tab_id),
        Some(b)
    );
}

#[test]
fn focus_activation_reflows_the_expanded_member_pty() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, a);
    add_tab(&mut session, tab_id, a);
    add_client(&mut session, client_id, tab_id, Some(a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Split off a live pane `b`, then stack `c` onto it: the stack is
    // [b, c] with `c` active and `b` collapsed to a header.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    match rt.dispatch(env) {
        CommandResult::Ok { .. } => {}
        other => panic!("expected Ok, got {other:?}"),
    }
    let b = other_pane(&rt, sid, a);
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            stacked: true,
            ..NewPaneArgs::default()
        }),
    );
    match rt.dispatch(env) {
        CommandResult::Ok { .. } => {}
        other => panic!("expected Ok, got {other:?}"),
    }
    let c = {
        let mut others = rt.sessions[&sid]
            .panes
            .list()
            .map(PaneRecord::id)
            .filter(|id| *id != a && *id != b);
        let pane = others.next().expect("a third pane exists");
        assert!(others.next().is_none(), "exactly one new pane");
        pane
    };
    let c_resizes_before = fake.resizes(c).expect("c spawned").len();

    // Focusing collapsed `b` expands it: 80x24 leaves an 80x22 pane region;
    // the half-width stack member has one header, so content becomes 38x19.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(b),
            client: None,
        }),
    );
    match rt.dispatch(env) {
        CommandResult::Ok { emitted_events, .. } => {
            // LayoutChanged + PtyResized(b) + PaneFocused. `c` collapses to a
            // header and keeps its last PTY size, so it is not resized.
            assert_eq!(emitted_events.len(), 3);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &rt.sessions[&sid];
    assert_eq!(
        session.tabs[&tab_id].layout(),
        &LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutChild::new(LayoutNode::Pane(a)),
                LayoutChild::new(LayoutNode::Split(SplitNode::stack(vec![b, c], 0))),
            ],
        ))
    );
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("client")
            .focused_pane(tab_id),
        Some(b)
    );
    assert_eq!(
        fake.resizes(b).expect("b spawned").last().copied(),
        Some(PtySize { cols: 38, rows: 19 })
    );
    // The newly collapsed `c` keeps its last PTY size: no resize reached it.
    assert_eq!(fake.resizes(c).expect("c spawned").len(), c_resizes_before);
}

#[test]
fn in_session_cli_source_pane_in_another_session_is_gone() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let session_a = SessionId::new();
    let pane_in_b = PaneId::new();

    let mut a = bare_session(session_a);
    add_client(&mut a, client_id, TabId::new(), None);
    rt.sessions.insert(a.id, a);

    // The pane lives in a *different* session; the acting session has no such
    // source pane, so the command is refused before any target resolution.
    let mut b = bare_session(SessionId::new());
    add_pane(&mut b, pane_in_b);
    rt.sessions.insert(b.id, b);

    let source = CommandSource::in_session_cli(
        session_a,
        Some(client_id),
        pane_in_b,
        PathBuf::from("/sock"),
    );
    let env = envelope_from(source, Command::ClosePane(ClosePaneArgs::default()));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetGone,
            help: Some("source pane no longer exists".to_string()),
        }
    );
}

#[test]
fn in_session_cli_pane_command_without_a_client_succeeds() {
    let (mut rt, _tx) = new_runtime();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(session_id);
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    rt.sessions.insert(session_id, session);

    // Grow a second pane; the split focuses it for the attached client.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, session_id, root);

    // The issuing pane was spawned with no designated client. Closing it is
    // pane-scoped, so no client is needed: the pane closes, and the attached
    // client's focus falls back to the root — PaneClosing + PaneRemoved +
    // LayoutChanged + PaneFocused.
    let source = CommandSource::in_session_cli(session_id, None, new_pane, PathBuf::from("/sock"));
    let env = envelope_from(source, Command::ClosePane(ClosePaneArgs::default()));
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert!(rt.sessions[&session_id].panes.get(new_pane).is_none());
}

#[test]
fn in_session_cli_pane_command_with_a_detached_client_succeeds() {
    let (mut rt, _tx) = new_runtime();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(session_id);
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    rt.sessions.insert(session_id, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, session_id, root);

    // The client that spawned the pane is long gone (never attached here).
    // The pane outlives it: a pane-scoped command from that pane still works.
    let stranger = ClientId::new();
    let source =
        CommandSource::in_session_cli(session_id, Some(stranger), new_pane, PathBuf::from("/sock"));
    let env = envelope_from(source, Command::ClosePane(ClosePaneArgs::default()));
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert!(rt.sessions[&session_id].panes.get(new_pane).is_none());
}

#[test]
fn in_session_cli_client_scoped_with_no_attached_client_is_stale() {
    let (mut rt, _tx) = new_runtime();
    let session_id = SessionId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(session_id);
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    rt.sessions.insert(session_id, session);

    // Lock mode is one client's own state, and no client is attached to stand
    // in for the one this pane never had.
    let source = CommandSource::in_session_cli(session_id, None, root, PathBuf::from("/sock"));
    let env = envelope_from(
        source,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: Some("no client is attached to the session".to_string()),
        }
    );
}

#[test]
fn in_session_cli_from_a_closing_pane_is_gone() {
    let (mut rt, _tx) = new_runtime();
    let session_id = SessionId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(session_id);
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    // The pane is mid-teardown: a command issued from it must not steer the
    // session anymore.
    session
        .panes
        .get_mut(root)
        .expect("record")
        .update_lifecycle(PaneLifecycleEvent::CloseRequested {
            since: SystemTime::now(),
        })
        .expect("spawning pane accepts a close request");
    rt.sessions.insert(session_id, session);

    let source = CommandSource::in_session_cli(session_id, None, root, PathBuf::from("/sock"));
    let env = envelope_from(source, Command::RenamePane(RenamePaneArgs { pane: None }));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetGone,
            help: Some("source pane no longer exists".to_string()),
        }
    );
}

#[test]
fn in_session_cli_from_an_exited_pane_is_a_valid_source() {
    let (mut rt, _tx) = new_runtime();
    let session_id = SessionId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(session_id);
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    // The pane's child ran and exited; its close policy keeps it on screen.
    // A background child it left behind can still command from it.
    {
        let record = session.panes.get_mut(root).expect("record");
        record
            .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
            .expect("spawning pane starts");
        record
            .update_lifecycle(PaneLifecycleEvent::ProcessExited {
                code: Some(0),
                at: SystemTime::now(),
            })
            .expect("running pane exits");
    }
    rt.sessions.insert(session_id, session);

    let source = CommandSource::in_session_cli(session_id, None, root, PathBuf::from("/sock"));
    let env = envelope_from(source, Command::RenamePane(RenamePaneArgs { pane: None }));
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
}

#[test]
fn mouse_select_cannot_be_issued_from_the_cli() {
    // The CLI has no mouse-select verb; the command is refused before any
    // state is read, so even an empty runtime answers.
    let (mut rt, _tx) = new_runtime();
    let source = CommandSource::in_session_cli(
        SessionId::new(),
        Some(ClientId::new()),
        PaneId::new(),
        PathBuf::from("/sock"),
    );
    let env = envelope_from(source, Command::ToggleMouseSelect);
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::Unauthorized,
            help: Some("command cannot be issued from the CLI".to_string()),
        }
    );
}

#[test]
fn copy_cannot_be_issued_from_the_cli() {
    let (mut rt, _tx) = new_runtime();
    let source = CommandSource::in_session_cli(
        SessionId::new(),
        Some(ClientId::new()),
        PaneId::new(),
        PathBuf::from("/sock"),
    );
    let env = envelope_from(
        source,
        Command::Visual(VisualCommand::Copy(CopyArgs {
            target: CopyTarget::Osc52,
        })),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::Unauthorized,
            help: Some("command cannot be issued from the CLI".to_string()),
        }
    );
}

#[test]
fn quit_cannot_be_issued_from_inside_a_pane() {
    let (mut rt, _tx) = new_runtime();
    let source = CommandSource::in_session_cli(
        SessionId::new(),
        Some(ClientId::new()),
        PaneId::new(),
        PathBuf::from("/sock"),
    );
    let env = envelope_from(source, Command::Quit);
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::Unauthorized,
            help: Some("command cannot be issued from the CLI".to_string()),
        }
    );
    assert!(!rt.quit_requested());
}

#[test]
fn quit_from_an_external_cli_is_accepted() {
    // `kill-session` sends `Quit` from outside the session.
    let (mut rt, _tx) = new_runtime();
    let source = CommandSource::external_cli(None);
    let env = envelope_from(source, Command::Quit);
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert!(rt.quit_requested());
}

#[test]
fn in_session_cli_with_an_unknown_session_is_not_found() {
    // The envelope names a session this runtime does not run.
    let (mut rt, _tx) = new_runtime();
    let source = CommandSource::in_session_cli(
        SessionId::new(),
        Some(ClientId::new()),
        PaneId::new(),
        PathBuf::from("/sock"),
    );
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
fn external_cli_default_pane_with_no_attached_client_is_stale() {
    let (mut rt, _tx) = new_runtime();
    let session_id = SessionId::new();
    rt.sessions.insert(session_id, bare_session(session_id));

    // A session resolves, but its focused-pane default acts through the
    // acting client, and a session with nobody attached has none.
    let source = CommandSource::external_cli(Some(session_id));
    let env = envelope_from(source, Command::ClosePane(ClosePaneArgs::default()));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: Some("no client is attached to the session".to_string()),
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

    // Fullscreen defaults through the focused pane; it is outside the tab.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::TogglePaneFullscreen,
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

    // A session alone is not enough — RunCommandPane splits the acting
    // client's focused pane, like NewPane, and a session with nobody
    // attached has no acting client.
    let source = CommandSource::external_cli(Some(session_id));
    let env = envelope_from(
        source,
        Command::RunCommandPane(RunCommandPaneArgs {
            command: spawn_spec(),
            cwd: None,
            source: None,
            tab: None,
            direction: None,
            stacked: false,
            client: None,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: Some("no client is attached to the session".to_string()),
        }
    );
}

#[test]
fn run_command_pane_spawns_and_records_the_command() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Splits the focused pane and spawns the requested command in the new pane.
    match rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::RunCommandPane(RunCommandPaneArgs {
            command: spawn_spec(),
            cwd: None,
            source: None,
            tab: None,
            direction: None,
            stacked: false,
            client: None,
        }),
    )) {
        CommandResult::Ok { .. } => {}
        other => panic!("expected Ok, got {other:?}"),
    }

    // The command is spawned verbatim — save for koshi's terminal identity
    // and the in-session identity vars added to its environment — and
    // recorded on the pane without the identity vars, taking the default
    // close-on-exit policy.
    let new_pane = other_pane(&rt, sid, root);
    let mut recorded = spawn_spec();
    recorded.env = rt.terminal_identity_env(BTreeMap::new());
    let mut launched = recorded.clone();
    launched.env.extend(koshi_env(
        sid,
        Some(client_id),
        new_pane,
        koshi_paths::runtime_dir().as_deref(),
    ));
    assert_eq!(fake.spawn_spec(new_pane).unwrap(), launched);
    let record = rt.sessions[&sid].panes.get(new_pane).unwrap();
    assert_eq!(record.command, Some(recorded));
    assert_eq!(record.exit_policy, PaneExitPolicy::CloseOnExit);
    // The new command pane is focused for the issuing client.
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .focused_pane(tab),
        Some(new_pane)
    );
}

#[test]
fn run_command_pane_carries_cwd_into_the_command() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // The command carries no cwd of its own, so the args `cwd` fills it in.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::RunCommandPane(RunCommandPaneArgs {
            command: spawn_spec(),
            cwd: Some(PathBuf::from("/work")),
            source: None,
            tab: None,
            direction: None,
            stacked: false,
            client: None,
        }),
    ));

    // `--cwd` reaches the spawned child and is recorded as the pane's directory.
    let new_pane = other_pane(&rt, sid, root);
    assert_eq!(
        fake.spawn_spec(new_pane).unwrap().cwd,
        Some(PathBuf::from("/work"))
    );
    assert_eq!(
        rt.sessions[&sid].panes.get(new_pane).unwrap().cwd,
        Some(PathBuf::from("/work"))
    );
}

#[test]
fn run_command_pane_args_carry_placement_into_the_new_pane_mapping() {
    // The run → new-pane mapping forwards the source pane and every
    // placement field verbatim; only the command is made mandatory.
    let source = PaneId::new();
    let tab = TabId::new();
    let client = ClientId::new();
    let args = RunCommandPaneArgs {
        command: spawn_spec(),
        cwd: Some(PathBuf::from("/work")),
        source: Some(source),
        tab: Some(tab),
        direction: Some(Direction::Down),
        stacked: true,
        client: Some(client),
    };
    assert_eq!(
        Server::run_command_new_pane_args(&args),
        NewPaneArgs {
            source: Some(source),
            tab: Some(tab),
            direction: Some(Direction::Down),
            stacked: true,
            cwd: Some(PathBuf::from("/work")),
            command: Some(spawn_spec()),
            client: Some(client),
        }
    );
}

#[test]
fn in_session_cli_session_id_is_authoritative_over_a_mismatched_client() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let claimed_session = SessionId::new();
    let source_pane = PaneId::new();
    let tab = TabId::new();

    // The envelope claims session A, but the client is attached to session B.
    // Session A is looked up by its id, and a client-scoped command checks the
    // client *there* — it is not attached to A, so the command is rejected
    // rather than silently acting on B.
    let mut a = bare_session(claimed_session);
    add_pane(&mut a, source_pane);
    add_tab(&mut a, tab, source_pane);
    rt.sessions.insert(claimed_session, a);
    let mut b = bare_session(SessionId::new());
    add_client(&mut b, client_id, TabId::new(), None);
    rt.sessions.insert(b.id, b);

    let source = CommandSource::in_session_cli(
        claimed_session,
        Some(client_id),
        source_pane,
        PathBuf::from("/sock"),
    );
    let env = envelope_from(
        source,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: Some("no client is attached to the session".to_string()),
        }
    );
}

#[test]
fn focus_tab_next_in_a_single_tab_session_is_a_clean_noop() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, None);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Next wraps the single tab back onto itself: the target resolves to the
    // already-active tab, so nothing changes and no events are emitted.
    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Next,
            client: None,
        }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert!(emitted_events.is_empty()),
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .active_tab(),
        tab
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
            client: None,
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
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(pane),
            client: None,
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

    // CloseTab with no explicit tab — InSessionCli resolves via the tab
    // containing pane_a (tab A), not the client's active tab (tab B).
    let source =
        CommandSource::in_session_cli(session_id, Some(client_id), pane_a, PathBuf::from("/sock"));
    let result = rt.dispatch(envelope_from(
        source,
        Command::CloseTab(CloseTabArgs::default()),
    ));
    assert!(matches!(result, CommandResult::Ok { .. }));

    // Tab A — the source pane's tab — was closed; the client's active tab B
    // is untouched.
    let session = &rt.sessions[&session_id];
    assert!(!session.tabs.contains_key(&tab_a));
    assert!(session.tabs.contains_key(&tab_b));
    assert!(session.panes.get(pane_a).is_none());
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), tab_b);
}

#[test]
fn in_session_cli_tab_default_with_removed_source_pane_is_gone() {
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
    let source = CommandSource::in_session_cli(
        session_id,
        Some(client_id),
        stale_pane,
        PathBuf::from("/sock"),
    );
    let env = envelope_from(source, Command::CloseTab(CloseTabArgs::default()));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetGone,
            help: Some("source pane no longer exists".to_string()),
        }
    );
}

#[test]
fn new_pane_spawns_and_runs_the_child() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    let command_id = env.id;
    let result = rt.dispatch(env);

    let new_pane = other_pane(&rt, sid, root);
    assert!(matches!(result, CommandResult::Ok { command_id: id, .. } if id == command_id));
    // The child was spawned under the pane's own id and advanced to Running.
    assert_eq!(fake.spawned_panes(), vec![new_pane]);
    assert_eq!(
        *rt.sessions[&sid].panes.get(new_pane).unwrap().lifecycle(),
        PaneLifecycle::Running
    );
    // Its handle is parked so the reader thread keeps feeding output.
    assert!(rt.pty_handles.contains_key(&new_pane));
    // The root has no PTY yet, so it is neither spawned nor parked.
    assert!(!rt.pty_handles.contains_key(&root));
}

#[test]
fn new_pane_without_command_spawns_the_default_shell() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));

    // command: None resolves to the platform default shell carrying koshi's
    // terminal identity and the in-session identity vars in its environment.
    let new_pane = other_pane(&rt, sid, root);
    let mut expected = rt.default_shell_spec(None, BTreeMap::new());
    expected.env.extend(koshi_env(
        sid,
        Some(client_id),
        new_pane,
        koshi_paths::runtime_dir().as_deref(),
    ));
    assert_eq!(fake.spawn_spec(new_pane).unwrap(), expected);
}

#[test]
fn new_pane_with_command_spawns_that_command() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            command: Some(spawn_spec()),
            ..NewPaneArgs::default()
        }),
    ));

    // An explicit command is spawned verbatim, save for koshi's terminal
    // identity and the in-session identity vars added to its environment.
    let new_pane = other_pane(&rt, sid, root);
    let mut expected = spawn_spec();
    expected.env = rt.terminal_identity_env(BTreeMap::new());
    expected.env.extend(koshi_env(
        sid,
        Some(client_id),
        new_pane,
        koshi_paths::runtime_dir().as_deref(),
    ));
    assert_eq!(fake.spawn_spec(new_pane).unwrap(), expected);
}

#[test]
fn new_pane_spawn_failure_leaves_no_trace() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    fake.fail_spawns_with(PtyError::Spawn {
        detail: "boom".to_string(),
    });
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);
    let before_layout = rt.sessions[&sid].tabs[&tab].layout().clone();

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    let command_id = env.id;

    // The child cannot launch, so the command rejects and nothing is committed.
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("failed to launch the pane's process".to_string()),
        }
    );
    // No new pane, the layout is untouched, no handle was parked, and the
    // client's focus never moved to a pane that never existed.
    assert_eq!(rt.sessions[&sid].panes.len(), 1);
    assert!(rt.sessions[&sid].panes.get(root).is_some());
    assert_eq!(rt.sessions[&sid].tabs[&tab].layout(), &before_layout);
    assert!(rt.pty_handles.is_empty());
    assert!(fake.spawned_panes().is_empty());
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .focused_pane(tab),
        Some(root)
    );
}

#[test]
fn new_pane_adoption_spawn_failure_leaves_no_trace() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    fake.fail_spawns_with(PtyError::Spawn {
        detail: "boom".to_string(),
    });
    let client_id = ClientId::new();
    let tab_front = TabId::new();
    let tab_back = TabId::new();
    let pane_front = PaneId::new();
    let pane_back = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_front);
    add_pane(&mut session, pane_back);
    add_tab(&mut session, tab_front, pane_front);
    add_tab(&mut session, tab_back, pane_back);
    add_client(&mut session, client_id, tab_front, Some(pane_front));
    let sid = session.id;
    rt.sessions.insert(sid, session);
    let before_layout = rt.sessions[&sid].tabs[&tab_back].layout().clone();

    // The split would adopt the client onto the background tab, but the spawn
    // happens first and fails — so the adoption never occurs: the client stays on
    // the front tab and no pane appears.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_back),
            ..NewPaneArgs::default()
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("failed to launch the pane's process".to_string()),
        }
    );
    let session = &rt.sessions[&sid];
    assert_eq!(
        session.clients.get(client_id).unwrap().active_tab(),
        tab_front
    );
    assert_eq!(session.panes.len(), 2);
    assert_eq!(session.tabs[&tab_back].layout(), &before_layout);
    assert!(rt.pty_handles.is_empty());
    assert!(fake.spawned_panes().is_empty());
}

#[test]
fn new_pane_on_a_background_tab_adopts_a_viewer() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();

    // One session, one client viewing the front tab. A second, background tab
    // holds `pane_back` and has no viewer, so it has no viewport of its own.
    let client_id = ClientId::new();
    let tab_front = TabId::new();
    let tab_back = TabId::new();
    let pane_front = PaneId::new();
    let pane_back = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_front);
    add_pane(&mut session, pane_back);
    add_tab(&mut session, tab_front, pane_front);
    add_tab(&mut session, tab_back, pane_back);
    add_client(&mut session, client_id, tab_front, Some(pane_front));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_back),
            ..NewPaneArgs::default()
        }),
    ));

    // No client was viewing the background tab, so the sole client is adopted
    // onto it: it switches to view that tab, the split spawns like any in-view
    // one, and the adopted client focuses the new pane. Events: TabFocused,
    // PaneCreated, LayoutChanged, PaneFocused, PtyResized (the PTY-less
    // `pane_back` sibling is skipped by the reflow).
    let new_pane = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_front && *id != pane_back)
        .expect("the freshly split pane");
    match result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 5),
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &rt.sessions[&sid];
    assert_eq!(
        session.clients.get(client_id).unwrap().active_tab(),
        tab_back
    );
    assert_eq!(
        session
            .clients
            .get(client_id)
            .unwrap()
            .focused_pane(tab_back),
        Some(new_pane)
    );
    assert!(rt.pty_handles.contains_key(&new_pane));
    assert!(fake.spawned_panes().contains(&new_pane));
    assert_eq!(
        *rt.sessions[&sid].panes.get(new_pane).unwrap().lifecycle(),
        PaneLifecycle::Running
    );
}

#[test]
fn new_pane_on_a_background_tab_adopts_the_issuing_client() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();

    // Two clients in one session, both on the front tab. The issuer is chosen as
    // the *higher*-id client, so the earliest-id fallback (`.min()`) would pick
    // the other one — proving adoption prefers the issuer, not the lowest id.
    let c1 = ClientId::new();
    let c2 = ClientId::new();
    let (issuer, bystander) = if c1 < c2 { (c2, c1) } else { (c1, c2) };
    let tab_front = TabId::new();
    let tab_back = TabId::new();
    let pane_front = PaneId::new();
    let pane_back = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_front);
    add_pane(&mut session, pane_back);
    add_tab(&mut session, tab_front, pane_front);
    add_tab(&mut session, tab_back, pane_back);
    add_client(&mut session, bystander, tab_front, Some(pane_front));
    add_client(&mut session, issuer, tab_front, Some(pane_front));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // The issuer splits the background tab; it — not the lower-id bystander — is
    // pulled onto the tab and focuses the new pane.
    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(issuer),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_back),
            ..NewPaneArgs::default()
        }),
    ));
    // TabFocused, PaneCreated, LayoutChanged, PaneFocused, PtyResized.
    match result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 5),
        other => panic!("expected Ok, got {other:?}"),
    }

    let new_pane = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_front && *id != pane_back)
        .expect("the freshly split pane");
    let session = &rt.sessions[&sid];
    assert_eq!(session.clients.get(issuer).unwrap().active_tab(), tab_back);
    assert_eq!(
        session.clients.get(issuer).unwrap().focused_pane(tab_back),
        Some(new_pane)
    );
    // The bystander was left exactly where it was.
    assert_eq!(
        session.clients.get(bystander).unwrap().active_tab(),
        tab_front
    );
    assert_eq!(
        session
            .clients
            .get(bystander)
            .unwrap()
            .focused_pane(tab_back),
        None
    );
    assert!(rt.pty_handles.contains_key(&new_pane));
}

#[test]
fn new_pane_external_multiple_clients_is_ambiguous() {
    let (mut rt, _tx) = new_runtime();

    let tab_front = TabId::new();
    let tab_back = TabId::new();
    let pane_front = PaneId::new();
    let pane_back = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_front);
    add_pane(&mut session, pane_back);
    add_tab(&mut session, tab_front, pane_front);
    add_tab(&mut session, tab_back, pane_back);
    let c1 = ClientId::new();
    let c2 = ClientId::new();
    add_client(&mut session, c1, tab_front, Some(pane_front));
    add_client(&mut session, c2, tab_front, Some(pane_front));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // No issuing client (external) and the background tab has no viewer, but the
    // session has two attached clients — adopting either would hijack a bystander,
    // so it rejects and asks for a named target, changing nothing.
    let env = envelope_from(
        CommandSource::external_cli(Some(sid)),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_back),
            ..NewPaneArgs::default()
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some("multiple clients; name a target client for the new pane".to_string()),
        }
    );
    let session = &rt.sessions[&sid];
    assert_eq!(session.panes.len(), 2);
    assert_eq!(session.clients.get(c1).unwrap().active_tab(), tab_front);
    assert_eq!(session.clients.get(c2).unwrap().active_tab(), tab_front);
}

#[test]
fn new_pane_external_targets_a_named_client() {
    let (mut rt, _tx) = new_runtime();

    let tab_front = TabId::new();
    let tab_back = TabId::new();
    let pane_front = PaneId::new();
    let pane_back = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_front);
    add_pane(&mut session, pane_back);
    add_tab(&mut session, tab_front, pane_front);
    add_tab(&mut session, tab_back, pane_back);
    let bystander = ClientId::new();
    let target = ClientId::new();
    add_client(&mut session, bystander, tab_front, Some(pane_front));
    add_client(&mut session, target, tab_front, Some(pane_front));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // External CLI names `target`: it is adopted onto the background tab and
    // focuses the new pane; the bystander is left untouched.
    rt.dispatch(envelope_from(
        CommandSource::external_cli(Some(sid)),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_back),
            client: Some(target),
            ..NewPaneArgs::default()
        }),
    ));
    let new_pane = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_front && *id != pane_back)
        .expect("the freshly split pane");
    let session = &rt.sessions[&sid];
    assert_eq!(session.clients.get(target).unwrap().active_tab(), tab_back);
    assert_eq!(
        session.clients.get(target).unwrap().focused_pane(tab_back),
        Some(new_pane)
    );
    assert_eq!(
        session.clients.get(bystander).unwrap().active_tab(),
        tab_front
    );
    assert!(rt.pty_handles.contains_key(&new_pane));
}

#[test]
fn new_pane_external_unattached_target_client_is_not_found() {
    let (mut rt, _tx) = new_runtime();

    let tab_front = TabId::new();
    let tab_back = TabId::new();
    let pane_front = PaneId::new();
    let pane_back = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_front);
    add_pane(&mut session, pane_back);
    add_tab(&mut session, tab_front, pane_front);
    add_tab(&mut session, tab_back, pane_back);
    let client_id = ClientId::new();
    add_client(&mut session, client_id, tab_front, Some(pane_front));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // The named client is not attached to the session: reject before mutating.
    let ghost = ClientId::new();
    let env = envelope_from(
        CommandSource::external_cli(Some(sid)),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_back),
            client: Some(ghost),
            ..NewPaneArgs::default()
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    assert_eq!(rt.sessions[&sid].panes.len(), 2);
}

#[test]
fn new_pane_explicit_client_wins_over_the_in_session_issuer() {
    let (mut rt, _tx) = new_runtime();
    let issuer = ClientId::new();
    let other = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, issuer, tab, Some(root));
    add_client(&mut session, other, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // The issuer runs the command but names `other` as the target client: the
    // explicit `--client` wins even in-session, so `other` focuses the new pane
    // and the issuer's focus is left untouched.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(issuer),
        Command::NewPane(NewPaneArgs {
            client: Some(other),
            ..NewPaneArgs::default()
        }),
    ));
    let new_pane = other_pane(&rt, sid, root);
    let session = &rt.sessions[&sid];
    assert_eq!(
        session.clients.get(other).unwrap().focused_pane(tab),
        Some(new_pane)
    );
    assert_eq!(
        session.clients.get(issuer).unwrap().focused_pane(tab),
        Some(root)
    );
}

#[test]
fn new_pane_explicit_unattached_client_is_rejected_even_with_an_issuer() {
    let (mut rt, _tx) = new_runtime();
    let issuer = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, issuer, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // A wrong `--client` (not attached) rejects outright — no fallback to the
    // issuing client, even though one is present.
    let ghost = ClientId::new();
    let env = envelope_from(
        CommandSource::key_binding(issuer),
        Command::NewPane(NewPaneArgs {
            client: Some(ghost),
            ..NewPaneArgs::default()
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    assert_eq!(rt.sessions[&sid].panes.len(), 1);
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(issuer)
            .unwrap()
            .focused_pane(tab),
        Some(root)
    );
}

#[test]
fn new_pane_wont_fit_on_a_background_tab_changes_nothing() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();

    // One client viewing the front tab at a 2x1 viewport too small to split.
    let client_id = ClientId::new();
    let tab_front = TabId::new();
    let tab_back = TabId::new();
    let pane_front = PaneId::new();
    let pane_back = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_front);
    add_pane(&mut session, pane_back);
    add_tab(&mut session, tab_front, pane_front);
    add_tab(&mut session, tab_back, pane_back);
    let mut client = Client::new(
        client_id,
        session.id,
        SystemTime::now(),
        Size { cols: 2, rows: 1 },
        tab_front,
    );
    client.update_focused_pane(tab_front, pane_front);
    session.attach_client(client);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // The split would size against the 2x1 viewport of the client that would be
    // adopted, but fit is checked before anything mutates: it cannot fit, so the
    // command rejects and nothing changes — the client is never moved.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_back),
            ..NewPaneArgs::default()
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("not enough space for a new pane".to_string()),
        }
    );
    let session = &rt.sessions[&sid];
    // The client never left its tab, nothing spawned, no pane added.
    assert_eq!(
        session.clients.get(client_id).unwrap().active_tab(),
        tab_front
    );
    assert_eq!(session.panes.len(), 2);
    assert!(fake.spawned_panes().is_empty());
    assert!(rt.pty_handles.is_empty());
}

#[test]
fn new_pane_adoption_reflows_the_vacated_tab() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();

    let tab_front = TabId::new();
    let tab_back = TabId::new();
    let pane_front = PaneId::new();
    let pane_back = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_front);
    add_pane(&mut session, pane_back);
    add_tab(&mut session, tab_front, pane_front);
    add_tab(&mut session, tab_back, pane_back);

    // Client A (the issuer) is the smaller, size-constraining viewer of the front
    // tab; client B is the larger 80-wide viewer. Both view the front tab.
    let client_a = ClientId::new();
    let mut a = Client::new(
        client_a,
        session.id,
        SystemTime::now(),
        Size { cols: 40, rows: 10 },
        tab_front,
    );
    a.update_focused_pane(tab_front, pane_front);
    session.attach_client(a);
    let client_b = ClientId::new();
    add_client(&mut session, client_b, tab_front, Some(pane_front));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Split the front tab (from B) so it holds a live PTY sized to A's 40-wide
    // constraint.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_b),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_x = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_front && *id != pane_back)
        .expect("the front-tab split pane");
    let before = fake.resizes(pane_x).unwrap();

    // A issues a split against the background tab: A is adopted onto it and leaves
    // the front tab, whose viewport now grows to B's 80 wide. The front tab's
    // live PTY is reflowed exactly once, larger than before.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_a),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_back),
            ..NewPaneArgs::default()
        }),
    ));
    let after = fake.resizes(pane_x).unwrap();
    assert_eq!(after.len(), before.len() + 1);
    assert!(after.last().unwrap().cols > before.last().unwrap().cols);
}

#[test]
fn new_pane_adoption_vacated_tab_with_no_viewer_keeps_sizes() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_front = TabId::new();
    let tab_back = TabId::new();
    let pane_front = PaneId::new();
    let pane_back = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_front);
    add_pane(&mut session, pane_back);
    add_tab(&mut session, tab_front, pane_front);
    add_tab(&mut session, tab_back, pane_back);
    add_client(&mut session, client_id, tab_front, Some(pane_front));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Give the front tab a live PTY pane by splitting it while viewed.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_x = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_front && *id != pane_back)
        .expect("the front-tab split pane");
    let before = fake.resizes(pane_x).unwrap().len();

    // The sole viewer is adopted onto the background tab, leaving the front tab
    // with no viewer: its live PTY keeps its size — not resized at all.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_back),
            ..NewPaneArgs::default()
        }),
    ));
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .active_tab(),
        tab_back
    );
    assert_eq!(fake.resizes(pane_x).unwrap().len(), before);
}

#[test]
fn new_pane_adoption_reflows_a_stale_sized_background_sibling() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let tab_back = TabId::new();
    let tab_front = TabId::new();
    let pane_back = PaneId::new();
    let pane_front = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_back);
    add_pane(&mut session, pane_front);
    add_tab(&mut session, tab_back, pane_back);
    add_tab(&mut session, tab_front, pane_front);
    // Client A (40 wide) views the back tab; client B (100 wide) views the front.
    let client_a = ClientId::new();
    let mut a = Client::new(
        client_a,
        session.id,
        SystemTime::now(),
        Size { cols: 40, rows: 10 },
        tab_back,
    );
    a.update_focused_pane(tab_back, pane_back);
    session.attach_client(a);
    let client_b = ClientId::new();
    let mut b = Client::new(
        client_b,
        session.id,
        SystemTime::now(),
        Size {
            cols: 100,
            rows: 50,
        },
        tab_front,
    );
    b.update_focused_pane(tab_front, pane_front);
    session.attach_client(b);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // A splits the back tab while it's the only (40-wide) viewer: the sibling's
    // PTY is sized to 40 wide.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_a),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_x = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_back && *id != pane_front)
        .expect("the back-tab split pane");
    let small = *fake.resizes(pane_x).unwrap().last().unwrap();

    // A leaves the back tab (now unviewed); its PTYs keep the stale 40-wide size.
    rt.sessions
        .get_mut(&sid)
        .unwrap()
        .clients
        .get_mut(client_a)
        .unwrap()
        .update_active_tab(tab_front);

    // B splits `pane_back` on the now-background tab: B is adopted at 100 wide. The
    // untouched sibling `pane_x` must be reflowed to the larger geometry, not left
    // at its stale 40-wide size.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_b),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_back),
            ..NewPaneArgs::default()
        }),
    ));
    let large = *fake.resizes(pane_x).unwrap().last().unwrap();
    assert!(
        large.cols > small.cols,
        "stale background sibling was reflowed to the larger viewport (was {small:?}, now {large:?})"
    );
}

#[test]
fn new_pane_external_sole_client_that_cannot_fit_is_min_size() {
    let (mut rt, _tx) = new_runtime();
    let tab_front = TabId::new();
    let tab_back = TabId::new();
    let pane_front = PaneId::new();
    let pane_back = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_front);
    add_pane(&mut session, pane_back);
    add_tab(&mut session, tab_front, pane_front);
    add_tab(&mut session, tab_back, pane_back);
    // The session's sole client is too small (2x1) to hold a split.
    let client_id = ClientId::new();
    let mut client = Client::new(
        client_id,
        session.id,
        SystemTime::now(),
        Size { cols: 2, rows: 1 },
        tab_front,
    );
    client.update_focused_pane(tab_front, pane_front);
    session.attach_client(client);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // An external CLI (no issuer) targets the unviewed background tab: the sole
    // client is the unambiguous default, but its viewport cannot hold the split,
    // so it rejects MinSize — before any mutation.
    let env = envelope_from(
        CommandSource::external_cli(Some(sid)),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_back),
            ..NewPaneArgs::default()
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("not enough space for a new pane".to_string()),
        }
    );
    assert_eq!(rt.sessions[&sid].panes.len(), 2);
}

#[test]
fn new_pane_leaves_an_unchanged_sibling_pty_alone() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Split 1: root -> (root | A), focus A. Split 2: A -> (root | (A | B)), focus B.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let a = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != root)
        .expect("pane A");
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let b = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != root && *id != a)
        .expect("pane B");
    let a_before = fake.resizes(a).unwrap().len();
    let b_before = fake.resizes(b).unwrap().len();

    // Split 3: B -> (root | (A | (B | C))). B shrinks and is reflowed; A's rect is
    // unchanged (its subtree is untouched), so A's PTY is left alone entirely.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    assert_eq!(fake.resizes(a).unwrap().len(), a_before);
    assert!(fake.resizes(b).unwrap().len() > b_before);
}

#[test]
fn new_pane_sibling_resize_failure_does_not_abort_the_command() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // First split spawns A with a live PTY.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let a = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != root)
        .expect("pane A");
    let a_before = fake.resizes(a).unwrap().len();
    // A's next resize will error.
    fake.fail_resizes_on(a, PtyError::UnknownPane { pane: a });

    // Second split reflows A (its resize errors), but best-effort means the
    // command still succeeds and the new pane B spawns; A's failed resize records
    // nothing.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    let command_id = env.id;
    let result = rt.dispatch(env);
    assert!(matches!(result, CommandResult::Ok { command_id: id, .. } if id == command_id));
    let b = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != root && *id != a)
        .expect("pane B");
    assert!(rt.pty_handles.contains_key(&b));
    assert_eq!(fake.resizes(a).unwrap().len(), a_before);
}

#[test]
fn new_pane_records_the_resolved_launch_cwd_on_the_command() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // `--cwd /work` with an explicit command whose own cwd is None: the command's
    // cwd resolves to /work at spawn.
    let mut command = spawn_spec();
    command.cwd = None;
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            cwd: Some(PathBuf::from("/work")),
            command: Some(command),
            ..NewPaneArgs::default()
        }),
    ));

    // The record's cwd, its command's own cwd, and the actual spawn cwd all agree
    // on the resolved launch directory — the record can't disagree with itself.
    let new_pane = other_pane(&rt, sid, root);
    let record = rt.sessions[&sid].panes.get(new_pane).unwrap();
    assert_eq!(record.cwd, Some(PathBuf::from("/work")));
    assert_eq!(
        record.command.as_ref().unwrap().cwd,
        Some(PathBuf::from("/work"))
    );
    assert_eq!(
        fake.spawn_spec(new_pane).unwrap().cwd,
        Some(PathBuf::from("/work"))
    );
}

#[test]
fn new_pane_records_an_explicit_commands_own_cwd() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // The command carries its own cwd (/cmd); `--cwd /work` must NOT override it.
    let mut command = spawn_spec();
    command.cwd = Some(PathBuf::from("/cmd"));
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            cwd: Some(PathBuf::from("/work")),
            command: Some(command),
            ..NewPaneArgs::default()
        }),
    ));

    // Record and spawn both use the command's own /cmd — the record can't disagree
    // with what the process actually launched with.
    let new_pane = other_pane(&rt, sid, root);
    let record = rt.sessions[&sid].panes.get(new_pane).unwrap();
    assert_eq!(record.cwd, Some(PathBuf::from("/cmd")));
    assert_eq!(
        record.command.as_ref().unwrap().cwd,
        Some(PathBuf::from("/cmd"))
    );
    assert_eq!(
        fake.spawn_spec(new_pane).unwrap().cwd,
        Some(PathBuf::from("/cmd"))
    );
}

#[test]
fn new_pane_default_shell_records_the_cwd_and_no_command() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // No command, `--cwd /work`: the default shell launches in /work; the record
    // stores that cwd and no command (the request named none).
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            cwd: Some(PathBuf::from("/work")),
            ..NewPaneArgs::default()
        }),
    ));

    let new_pane = other_pane(&rt, sid, root);
    let record = rt.sessions[&sid].panes.get(new_pane).unwrap();
    assert_eq!(record.cwd, Some(PathBuf::from("/work")));
    assert_eq!(record.command, None);
    assert_eq!(
        fake.spawn_spec(new_pane).unwrap().cwd,
        Some(PathBuf::from("/work"))
    );
}

#[test]
fn new_pane_external_into_a_viewed_tab_adopts_no_one() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // External CLI (no issuer, no --client) targeting a tab the client is already
    // viewing: the pane is created and sized to that viewer, but no one is
    // switched or focused — the client's focus stays on the root it was on.
    rt.dispatch(envelope_from(
        CommandSource::external_cli(Some(sid)),
        Command::NewPane(NewPaneArgs {
            source: Some(root),
            ..NewPaneArgs::default()
        }),
    ));
    let new_pane = other_pane(&rt, sid, root);
    let session = &rt.sessions[&sid];
    assert!(rt.pty_handles.contains_key(&new_pane));
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), tab);
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab),
        Some(root)
    );
}

#[test]
fn new_pane_reflows_existing_sibling_ptys() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // First split creates pane A and spawns its PTY (the root has none yet).
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_a = other_pane(&rt, sid, root);
    let a_resizes_before = fake.resizes(pane_a).unwrap().len();

    // Second split creates B off the now-focused A. A must reflow even though the
    // PTY-less root is in the layout — it must not abort the resize batch.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));

    // The second split reflows A exactly once more (spawn size + this reflow).
    assert_eq!(fake.resizes(pane_a).unwrap().len(), a_resizes_before + 1);
    // The root, having no PTY, was never resized — confirming it was skipped.
    assert!(fake.resizes(root).is_err());
}

#[test]
fn new_pane_explicit_command_inherits_the_pane_cwd() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // `--cwd /work` with an explicit command whose own cwd is None.
    let mut command = spawn_spec();
    command.cwd = None;
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            cwd: Some(PathBuf::from("/work")),
            command: Some(command),
            ..NewPaneArgs::default()
        }),
    ));

    // The command spawns in the pane cwd, not the inherited directory.
    let new_pane = other_pane(&rt, sid, root);
    assert_eq!(
        fake.spawn_spec(new_pane).unwrap().cwd,
        Some(PathBuf::from("/work"))
    );
}

#[test]
fn new_pane_explicit_command_cwd_wins_over_the_pane_cwd() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // The command sets its own cwd, so the pane cwd does not override it.
    let mut command = spawn_spec();
    command.cwd = Some(PathBuf::from("/cmd"));
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            cwd: Some(PathBuf::from("/work")),
            command: Some(command),
            ..NewPaneArgs::default()
        }),
    ));

    let new_pane = other_pane(&rt, sid, root);
    assert_eq!(
        fake.spawn_spec(new_pane).unwrap().cwd,
        Some(PathBuf::from("/cmd"))
    );
}

#[test]
fn new_pane_cross_session_sizes_to_a_target_session_viewer() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();

    // Session A holds the acting client.
    let client_a = ClientId::new();
    let id_a = SessionId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session_a = bare_session(id_a);
    add_pane(&mut session_a, pane_a);
    add_tab(&mut session_a, tab_a, pane_a);
    add_client(&mut session_a, client_a, tab_a, Some(pane_a));
    rt.sessions.insert(id_a, session_a);

    // Session B owns the target pane and has its own client viewing B's tab with
    // a viewport distinct from the no-client default.
    let client_b = ClientId::new();
    let id_b = SessionId::new();
    let tab_b = TabId::new();
    let pane_b = PaneId::new();
    let mut session_b = bare_session(id_b);
    add_pane(&mut session_b, pane_b);
    add_tab(&mut session_b, tab_b, pane_b);
    let mut viewer = Client::new(
        client_b,
        id_b,
        SystemTime::now(),
        Size { cols: 40, rows: 10 },
        tab_b,
    );
    viewer.update_focused_pane(tab_b, pane_b);
    session_b.attach_client(viewer);
    rt.sessions.insert(id_b, session_b);

    // Cross-session --pane from A's client: no focus client in B, but B has a
    // viewer, so the new pane sizes to B's viewport, not the 80x24 default.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_a),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_b),
            ..NewPaneArgs::default()
        }),
    ));

    let new_pane = other_pane(&rt, id_b, pane_b);
    assert!(rt.pty_handles.contains_key(&new_pane));
    // Exactly the size the production pipeline yields for a rightward split of
    // `pane_b` at B's 40x10 viewport — pins the target-viewer sizing and the
    // default split direction, not a loose bound (a vertical split would satisfy
    // any cols<=40/rows<=10 bound but produce a different exact size).
    let expected = {
        let probe = PaneId::new();
        let candidate =
            split_leaf(&LayoutNode::Pane(pane_b), pane_b, probe, Direction::Right).unwrap();
        let rects = content_rects(&solve_with_mode_min(
            &candidate,
            LayoutMode::Tiled,
            Rect::new(Point { x: 0, y: 0 }, Size { cols: 40, rows: 8 }),
            MIN_PANE_SIZE,
        ));
        let rect = rects
            .iter()
            .find(|(id, _)| *id == probe)
            .and_then(|(_, r)| *r)
            .expect("new pane has a content rect");
        compute_pty_size(rect)
    };
    assert_eq!(fake.resizes(new_pane).unwrap()[0], expected);
    assert_ne!(expected, PtySize { cols: 80, rows: 24 });
}

#[test]
fn close_pane_defaults_to_the_focused_pane_and_kills_gracefully() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, sid, root);

    // No explicit pane: the focused (split) pane closes. The root survives and
    // inherits focus — PaneClosing + PaneRemoved + LayoutChanged + PaneFocused.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(rt.sessions[&sid].panes.get(new_pane).is_none());
    assert_eq!(rt.sessions[&sid].panes.len(), 1);
    assert_eq!(
        rt.sessions[&sid].tabs[&tab].layout(),
        &LayoutNode::Pane(root)
    );
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .focused_pane(tab),
        Some(root)
    );
    assert!(!rt.pty_handles.contains_key(&new_pane));
    assert!(!rt.pty_sizes.contains_key(&new_pane));
    // The default close policy is a graceful kill with the standard window.
    assert_eq!(
        wait_for_kill(&fake, new_pane),
        vec![KillPolicy::Graceful {
            timeout: GRACEFUL_TIMEOUT_DURATION
        }]
    );
}

#[test]
fn close_pane_explicit_non_focused_target_keeps_focus() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, sid, root);

    // Close the non-focused root explicitly: nobody's focus needs repair, so
    // PaneClosing + PaneRemoved + LayoutChanged + PtyResized(the surviving
    // split pane, now full-tab) are emitted and the client stays on the split
    // pane.
    let env = envelope(Command::ClosePane(ClosePaneArgs {
        pane: Some(root),
        force: false,
        tree: false,
    }));
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(rt.sessions[&sid].panes.get(root).is_none());
    assert_eq!(
        rt.sessions[&sid].tabs[&tab].layout(),
        &LayoutNode::Pane(new_pane)
    );
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .focused_pane(tab),
        Some(new_pane)
    );
    assert!(rt.pty_handles.contains_key(&new_pane));
}

#[test]
fn close_pane_force_overrides_the_close_policy() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, sid, root);
    rt.sessions
        .get_mut(&sid)
        .unwrap()
        .panes
        .get_mut(new_pane)
        .unwrap()
        .close_policy = PaneClosePolicy::ConfirmIfBusy;

    // `--force` wins over the pane's own policy: the close applies and the
    // child is force-killed, no busy question asked.
    let env = envelope(Command::ClosePane(ClosePaneArgs {
        pane: Some(new_pane),
        force: true,
        tree: false,
    }));
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert!(rt.sessions[&sid].panes.get(new_pane).is_none());
    assert_eq!(wait_for_kill(&fake, new_pane), vec![KillPolicy::Force]);
}

#[test]
fn close_pane_tree_widens_the_graceful_kill_to_the_group() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, sid, root);

    // The default-bound close key sends `tree: true`: the pane's graceful
    // policy keeps its window, widened to the whole process group so every
    // descendant stops with the shell.
    let env = envelope(Command::ClosePane(ClosePaneArgs {
        pane: Some(new_pane),
        force: false,
        tree: true,
    }));
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert!(rt.sessions[&sid].panes.get(new_pane).is_none());
    assert_eq!(
        wait_for_kill(&fake, new_pane),
        vec![KillPolicy::GracefulTree {
            timeout: GRACEFUL_TIMEOUT_DURATION
        }]
    );
}

#[test]
fn close_pane_tree_with_force_group_kills_immediately() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, sid, root);

    let env = envelope(Command::ClosePane(ClosePaneArgs {
        pane: Some(new_pane),
        force: true,
        tree: true,
    }));
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert!(rt.sessions[&sid].panes.get(new_pane).is_none());
    assert_eq!(wait_for_kill(&fake, new_pane), vec![KillPolicy::Tree]);
}

#[test]
fn close_pane_confirm_if_busy_running_rejects_and_mutates_nothing() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, sid, root);
    rt.sessions
        .get_mut(&sid)
        .unwrap()
        .panes
        .get_mut(new_pane)
        .unwrap()
        .close_policy = PaneClosePolicy::ConfirmIfBusy;

    // The pane's child is `Running`, so busy cannot be ruled out: the close
    // rejects and neither state nor process is touched.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane may be busy; pass --force to close anyway".to_string()),
        }
    );

    assert_eq!(rt.sessions[&sid].panes.len(), 2);
    assert_eq!(
        rt.sessions[&sid].tabs[&tab].layout().leaf_panes(),
        vec![root, new_pane]
    );
    assert!(rt.pty_handles.contains_key(&new_pane));
    assert!(fake.kills(new_pane).unwrap().is_empty());
}

#[test]
fn close_pane_confirm_if_busy_exited_closes_gracefully() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, sid, root);
    {
        let record = rt
            .sessions
            .get_mut(&sid)
            .unwrap()
            .panes
            .get_mut(new_pane)
            .unwrap();
        record.close_policy = PaneClosePolicy::ConfirmIfBusy;
        record
            .update_lifecycle(PaneLifecycleEvent::ProcessExited {
                code: Some(0),
                at: SystemTime::now(),
            })
            .unwrap();
    }

    // An `Exited` child is provably not busy: the close proceeds gracefully.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(rt.sessions[&sid].panes.get(new_pane).is_none());
    assert_eq!(
        wait_for_kill(&fake, new_pane),
        vec![KillPolicy::Graceful {
            timeout: GRACEFUL_TIMEOUT_DURATION
        }]
    );
}

#[test]
fn close_pane_confirm_if_busy_spawning_rejects() {
    let (mut rt, _tx) = new_runtime();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    let sid = session.id;
    rt.sessions.insert(sid, session);
    rt.sessions
        .get_mut(&sid)
        .unwrap()
        .panes
        .get_mut(pane)
        .unwrap()
        .close_policy = PaneClosePolicy::ConfirmIfBusy;

    // A `Spawning` pane's child has not started, so busy cannot be ruled out
    // either: same rejection as `Running`.
    let env = envelope(Command::ClosePane(ClosePaneArgs {
        pane: Some(pane),
        force: false,
        tree: false,
    }));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane may be busy; pass --force to close anyway".to_string()),
        }
    );
    assert_eq!(rt.sessions[&sid].panes.len(), 1);
}

#[test]
fn close_pane_last_pane_closes_the_tab_and_quits() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    add_client(&mut session, client_id, tab, Some(pane));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Closing the only pane empties the tab, which closes; that was the last
    // tab, so the session winds down — PaneClosing + PaneRemoved + TabClosed +
    // Quit.
    let env = envelope(Command::ClosePane(ClosePaneArgs {
        pane: Some(pane),
        force: false,
        tree: false,
    }));
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(rt.sessions[&sid].tabs.is_empty());
    assert_eq!(rt.sessions[&sid].panes.len(), 0);
    assert_eq!(*rt.sessions[&sid].lifecycle(), SessionLifecycle::Stopping);
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .focused_pane(tab),
        None
    );
}

#[test]
fn close_pane_last_pane_of_a_tab_moves_viewers_to_the_nearest_tab() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, client_id, tab_b, Some(pane_b));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Emptying tab B closes it and moves its viewer to the surviving tab —
    // PaneClosing + PaneRemoved + TabClosed + TabFocused. The session keeps
    // running: another tab remains.
    let env = envelope(Command::ClosePane(ClosePaneArgs {
        pane: Some(pane_b),
        force: false,
        tree: false,
    }));
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(rt.sessions[&sid].tabs.len(), 1);
    assert!(rt.sessions[&sid].tabs.contains_key(&tab_a));
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .active_tab(),
        tab_a
    );
    // Unchanged from the setup: the session is not winding down.
    assert_eq!(*rt.sessions[&sid].lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn close_pane_unviewed_tab_repairs_stored_focus() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let pane_c = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_pane(&mut session, pane_c);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    // Tab B holds a two-member stack with `pane_c` expanded; the client views
    // tab A but remembers `pane_c` as its focus in tab B.
    session
        .tabs
        .get_mut(&tab_b)
        .unwrap()
        .update_layout(LayoutNode::Split(SplitNode::stack(vec![pane_b, pane_c], 1)));
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    // A second client with a smaller terminal, also viewing tab A: the
    // fallback viewport reduces across BOTH attached clients.
    let second_client = ClientId::new();
    let mut second = Client::new(
        second_client,
        session.id,
        SystemTime::now(),
        Size { cols: 60, rows: 20 },
        tab_a,
    );
    second.update_focused_pane(tab_a, pane_a);
    session.attach_client(second);
    let sid = session.id;
    rt.sessions.insert(sid, session);
    rt.sessions
        .get_mut(&sid)
        .unwrap()
        .clients
        .get_mut(client_id)
        .unwrap()
        .update_focused_pane(tab_b, pane_c);

    // Nobody views tab B, so its viewport falls back to the attached clients'
    // smallest; the stored focus entry still gets repaired onto the surviving
    // member — PaneClosing + PaneRemoved + LayoutChanged + PaneFocused.
    let env = envelope(Command::ClosePane(ClosePaneArgs {
        pane: Some(pane_c),
        force: false,
        tree: false,
    }));
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(
        rt.sessions[&sid].tabs[&tab_b].layout(),
        &LayoutNode::Pane(pane_b)
    );
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .focused_pane(tab_b),
        Some(pane_b)
    );
    // The client's view never moved.
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .active_tab(),
        tab_a
    );
}

#[test]
fn close_pane_reflows_surviving_pty_sizes() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Two splits: pane A (half width), then pane B splitting A (quarters).
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let pane_a = other_pane(&rt, sid, root);
    let size_at_half = rt.pty_sizes[&pane_a];
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let pane_b = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != root && *id != pane_a)
        .expect("a third pane exists");
    assert_ne!(rt.pty_sizes[&pane_a], size_at_half);

    // Closing B collapses the split back to [root | A]: A reclaims exactly the
    // half-width geometry it had before B existed, and its PTY is resized to
    // it — PaneClosing + PaneRemoved + LayoutChanged + PaneFocused +
    // PtyResized(A).
    let resizes_before = fake.resizes(pane_a).unwrap().len();
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 5);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(fake.resizes(pane_a).unwrap().len(), resizes_before + 1);
    assert_eq!(*fake.resizes(pane_a).unwrap().last().unwrap(), size_at_half);
    assert_eq!(rt.pty_sizes[&pane_a], size_at_half);
    assert!(rt.pty_handles.contains_key(&pane_a));
    assert!(!rt.pty_sizes.contains_key(&pane_b));
}

#[test]
fn close_pane_reflow_skips_a_survivor_whose_rect_is_unchanged() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // [root | A], then split root itself: [[root | B] | A]. A's right-half
    // rect is identical before and after B exists.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let pane_a = other_pane(&rt, sid, root);
    let size_a = rt.pty_sizes[&pane_a];
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source: Some(root),
            ..NewPaneArgs::default()
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(rt.pty_sizes[&pane_a], size_a);
    let resizes_a = fake.resizes(pane_a).unwrap().len();

    // Closing B restores [root | A]. A's rect never changed, so the reflow
    // leaves its PTY alone: PaneClosing + PaneRemoved + LayoutChanged +
    // PaneFocused only — no PtyResized at all (root has no PTY).
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(fake.resizes(pane_a).unwrap().len(), resizes_a);
    assert_eq!(rt.pty_sizes[&pane_a], size_a);
}

#[test]
fn close_pane_in_a_tab_with_no_viewer_keeps_pty_sizes() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_front = TabId::new();
    let tab_back = TabId::new();
    let root_front = PaneId::new();
    let pane_back = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root_front);
    add_pane(&mut session, pane_back);
    add_tab(&mut session, tab_front, root_front);
    add_tab(&mut session, tab_back, pane_back);
    add_client(&mut session, client_id, tab_front, Some(root_front));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Split the front tab while viewed so it holds a live PTY, then adopt the
    // sole viewer onto the back tab: the front tab is left with no viewer.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_x = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != root_front && *id != pane_back)
        .expect("the front-tab split pane");
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_back),
            ..NewPaneArgs::default()
        }),
    ));
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .active_tab(),
        tab_back
    );
    let resizes_x = fake.resizes(pane_x).unwrap().len();
    let size_x = rt.pty_sizes[&pane_x];

    // Closing the unviewed front tab's other pane frees space, but a tab with
    // no viewer has no viewport: the surviving PTY keeps its last size.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane: Some(root_front),
            force: false,
            tree: false,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert_eq!(
        rt.sessions[&sid].tabs[&tab_front].layout(),
        &LayoutNode::Pane(pane_x)
    );
    assert_eq!(fake.resizes(pane_x).unwrap().len(), resizes_x);
    assert_eq!(rt.pty_sizes[&pane_x], size_x);
}

#[test]
fn close_last_pane_reflows_the_tab_its_viewers_move_to() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let tab_main = TabId::new();
    let tab_solo = TabId::new();
    let root_main = PaneId::new();
    let pane_solo = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root_main);
    add_pane(&mut session, pane_solo);
    add_tab(&mut session, tab_main, root_main);
    add_tab(&mut session, tab_solo, pane_solo);
    // Client A (40x10) views the solo tab; client B (80x24) views the main tab.
    let client_a = ClientId::new();
    let mut a = Client::new(
        client_a,
        session.id,
        SystemTime::now(),
        Size { cols: 40, rows: 10 },
        tab_solo,
    );
    a.update_focused_pane(tab_solo, pane_solo);
    session.attach_client(a);
    let client_b = ClientId::new();
    add_client(&mut session, client_b, tab_main, Some(root_main));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Split the main tab (from B) so it holds a live PTY sized to B's 80-wide
    // viewport — A is not a viewer yet.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_b),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_y = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != root_main && *id != pane_solo)
        .expect("the main-tab split pane");
    let before = fake.resizes(pane_y).unwrap();

    // A closes its tab's only pane: the tab closes and A moves to the main
    // tab, whose viewport now reduces to A's 40x10 — its live PTY reflows
    // smaller. PaneClosing + PaneRemoved + TabClosed + TabFocused +
    // PtyResized(pane_y).
    let env = envelope_from(
        CommandSource::key_binding(client_a),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 5);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_a)
            .unwrap()
            .active_tab(),
        tab_main
    );
    let after = fake.resizes(pane_y).unwrap();
    assert_eq!(after.len(), before.len() + 1);
    assert!(after.last().unwrap().cols < before.last().unwrap().cols);
    assert_eq!(rt.pty_sizes[&pane_y], *after.last().unwrap());
}

#[test]
fn close_last_pane_of_an_unviewed_tab_reflows_nothing() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_front = TabId::new();
    let tab_back = TabId::new();
    let root_front = PaneId::new();
    let pane_back = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root_front);
    add_pane(&mut session, pane_back);
    add_tab(&mut session, tab_front, root_front);
    add_tab(&mut session, tab_back, pane_back);
    add_client(&mut session, client_id, tab_front, Some(root_front));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // A live PTY on the viewed front tab.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_x = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != root_front && *id != pane_back)
        .expect("the front-tab split pane");
    let resizes_x = fake.resizes(pane_x).unwrap().len();
    let size_x = rt.pty_sizes[&pane_x];

    // Closing the unviewed back tab's only pane closes that tab; no viewer
    // moved, so no tab's viewport changed and nothing reflows — PaneClosing +
    // PaneRemoved + TabClosed only.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane: Some(pane_back),
            force: false,
            tree: false,
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 3);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert!(!rt.sessions[&sid].tabs.contains_key(&tab_back));
    assert_eq!(fake.resizes(pane_x).unwrap().len(), resizes_x);
    assert_eq!(rt.pty_sizes[&pane_x], size_x);
}

#[test]
fn close_pane_reflow_skips_a_collapsed_stack_member() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // [root | A], then stack B onto A: [root | stack(A collapsed, B expanded)].
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_a = other_pane(&rt, sid, root);
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            stacked: true,
            ..NewPaneArgs::default()
        }),
    ));
    let pane_b = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != root && *id != pane_a)
        .expect("the stacked pane");
    let resizes_a = fake.resizes(pane_a).unwrap().len();
    let size_a = rt.pty_sizes[&pane_a];
    let resizes_b = fake.resizes(pane_b).unwrap().len();

    // Closing root hands the stack the full tab. The expanded member B
    // reflows wider; the collapsed member A has no content rect and keeps its
    // last size, with no event — PaneClosing + PaneRemoved + LayoutChanged +
    // PtyResized(B).
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane: Some(root),
            force: false,
            tree: false,
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let b_resizes = fake.resizes(pane_b).unwrap();
    assert_eq!(b_resizes.len(), resizes_b + 1);
    assert_eq!(rt.pty_sizes[&pane_b], *b_resizes.last().unwrap());
    assert_eq!(fake.resizes(pane_a).unwrap().len(), resizes_a);
    assert_eq!(rt.pty_sizes[&pane_a], size_a);
}

#[test]
fn close_pane_repairs_focus_for_every_client_focused_on_it() {
    let (mut rt, _tx) = new_runtime();
    let client_a = ClientId::new();
    let client_b = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_a, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_a),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, sid, root);

    // A second client also focuses the split pane.
    let mut second = Client::new(
        client_b,
        sid,
        SystemTime::now(),
        Size { cols: 80, rows: 24 },
        tab,
    );
    second.update_focused_pane(tab, new_pane);
    rt.sessions.get_mut(&sid).unwrap().attach_client(second);

    // Both clients focused the closed pane, so each gets its own repair —
    // PaneClosing + PaneRemoved + LayoutChanged + PaneFocused per client.
    let env = envelope_from(
        CommandSource::key_binding(client_a),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 5);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    for client_id in [client_a, client_b] {
        assert_eq!(
            rt.sessions[&sid]
                .clients
                .get(client_id)
                .unwrap()
                .focused_pane(tab),
            Some(root)
        );
    }
}

#[test]
fn close_pane_clears_only_the_gone_panes_view_state() {
    let (mut rt, _tx) = new_runtime();
    let client = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Split so the tab survives closing one pane; the split takes focus.
    let env = envelope_from(
        CommandSource::key_binding(client),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let split = other_pane(&rt, sid, root);

    // Scroll both panes up, and highlight text in the one about to close.
    {
        let scrolled = rt
            .sessions
            .get_mut(&sid)
            .unwrap()
            .clients
            .get_mut(client)
            .unwrap();
        scrolled.set_scroll_offset(split, 5);
        scrolled.set_scroll_offset(root, 3);
        scrolled.set_selection(
            split,
            Selection {
                kind: SelectionKind::Character,
                anchor: GridPos { row: 0, col: 0 },
                cursor: GridPos { row: 0, col: 4 },
            },
        );
    }

    // Close the focused (split) pane.
    let env = envelope_from(
        CommandSource::key_binding(client),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    let after = rt.sessions[&sid].clients.get(client).unwrap();
    // The closed pane's offset and highlight are dropped — a highlight over a
    // pane that no longer exists would keep holding a view of nothing. The
    // survivor's offset is untouched.
    assert_eq!(after.scroll_offset(split), 0);
    assert_eq!(after.selection(split), None);
    assert!(!after.is_view_held(split));
    assert_eq!(after.scroll_offset(root), 3);
    assert!(after.is_view_held(root));
}

#[test]
fn close_tab_clears_the_view_state_of_its_panes() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, client_id, tab_b, Some(pane_b));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Scroll the doomed tab's pane up and the survivor's too, and highlight text
    // in the doomed one.
    {
        let client = rt
            .sessions
            .get_mut(&sid)
            .unwrap()
            .clients
            .get_mut(client_id)
            .unwrap();
        client.set_scroll_offset(pane_b, 5);
        client.set_scroll_offset(pane_a, 3);
        client.set_selection(
            pane_b,
            Selection {
                kind: SelectionKind::Character,
                anchor: GridPos { row: 0, col: 0 },
                cursor: GridPos { row: 0, col: 4 },
            },
        );
    }

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab: Some(tab_b),
            force: false,
            tree: false,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    let client = rt.sessions[&sid].clients.get(client_id).unwrap();
    // Every pane of the closed tab loses its offset and its highlight; the
    // surviving tab's pane keeps its own offset.
    assert_eq!(client.scroll_offset(pane_b), 0);
    assert_eq!(client.selection(pane_b), None);
    assert!(!client.is_view_held(pane_b));
    assert_eq!(client.scroll_offset(pane_a), 3);
    assert!(client.is_view_held(pane_a));
}

#[test]
fn close_pane_stacked_member_collapses_the_stack() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            stacked: true,
            ..NewPaneArgs::default()
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, sid, root);

    // Closing the expanded stack member collapses the two-member stack back to
    // a plain leaf, and focus repairs onto the survivor.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(rt.sessions[&sid].panes.get(new_pane).is_none());
    assert_eq!(
        rt.sessions[&sid].tabs[&tab].layout(),
        &LayoutNode::Pane(root)
    );
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .focused_pane(tab),
        Some(root)
    );
}

#[test]
fn close_pane_with_no_attached_clients_succeeds() {
    let (mut rt, _tx) = new_runtime();
    let tab = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab, pane_a);
    session
        .tabs
        .get_mut(&tab)
        .unwrap()
        .update_layout(LayoutNode::Split(SplitNode::stack(vec![pane_a, pane_b], 1)));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // No client is attached anywhere, so the tab solves against the nominal
    // 80x24 viewport; with nobody's focus to repair, only PaneClosing +
    // PaneRemoved + LayoutChanged are emitted.
    let env = envelope(Command::ClosePane(ClosePaneArgs {
        pane: Some(pane_b),
        force: false,
        tree: false,
    }));
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 3);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(rt.sessions[&sid].panes.get(pane_b).is_none());
    assert_eq!(rt.sessions[&sid].panes.len(), 1);
    assert_eq!(
        rt.sessions[&sid].tabs[&tab].layout(),
        &LayoutNode::Pane(pane_a)
    );
}

#[test]
fn close_pane_honors_the_panes_own_force_policy() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, sid, root);
    rt.sessions
        .get_mut(&sid)
        .unwrap()
        .panes
        .get_mut(new_pane)
        .unwrap()
        .close_policy = PaneClosePolicy::Force;

    // Without `--force`, the pane's own configured policy decides: a `Force`
    // record force-kills the child.
    let env = envelope(Command::ClosePane(ClosePaneArgs {
        pane: Some(new_pane),
        force: false,
        tree: false,
    }));
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert!(rt.sessions[&sid].panes.get(new_pane).is_none());
    assert_eq!(wait_for_kill(&fake, new_pane), vec![KillPolicy::Force]);
}

#[test]
fn close_pane_explicit_target_in_another_session_closes_there() {
    let (mut rt, _tx) = new_runtime();
    let client_a = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b1 = PaneId::new();
    let pane_b2 = PaneId::new();

    let mut session_a = bare_session(SessionId::new());
    add_pane(&mut session_a, pane_a);
    add_tab(&mut session_a, tab_a, pane_a);
    add_client(&mut session_a, client_a, tab_a, Some(pane_a));
    let sid_a = session_a.id;
    rt.sessions.insert(sid_a, session_a);

    let mut session_b = bare_session(SessionId::new());
    add_pane(&mut session_b, pane_b1);
    add_pane(&mut session_b, pane_b2);
    add_tab(&mut session_b, tab_b, pane_b1);
    session_b
        .tabs
        .get_mut(&tab_b)
        .unwrap()
        .update_layout(LayoutNode::Split(SplitNode::stack(
            vec![pane_b1, pane_b2],
            1,
        )));
    let sid_b = session_b.id;
    rt.sessions.insert(sid_b, session_b);

    // An explicit pane target is global: issued by session A's client, it
    // closes the pane in its owning session B and leaves A untouched.
    let env = envelope_from(
        CommandSource::key_binding(client_a),
        Command::ClosePane(ClosePaneArgs {
            pane: Some(pane_b2),
            force: false,
            tree: false,
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 3);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(rt.sessions[&sid_b].panes.get(pane_b2).is_none());
    assert_eq!(
        rt.sessions[&sid_b].tabs[&tab_b].layout(),
        &LayoutNode::Pane(pane_b1)
    );
    assert_eq!(rt.sessions[&sid_a].panes.len(), 1);
    assert_eq!(
        rt.sessions[&sid_a].tabs[&tab_a].layout(),
        &LayoutNode::Pane(pane_a)
    );
    assert_eq!(
        rt.sessions[&sid_a]
            .clients
            .get(client_a)
            .unwrap()
            .focused_pane(tab_a),
        Some(pane_a)
    );
}

/// A horizontal two-pane split with equal weights, for building a tab's
/// layout directly.
fn side_by_side(left: PaneId, right: PaneId) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutChild::new(LayoutNode::Pane(left)),
            LayoutChild::new(LayoutNode::Pane(right)),
        ],
    ))
}

/// A session with one viewed tab whose pane was split once through dispatch,
/// so the new pane has a live PTY. Returns the runtime, fake backend, inbox
/// sender, ids, and the new pane's spawn-time PTY size.
fn resize_fixture() -> (
    Server,
    Arc<FakePtyBackend>,
    mpsc::Sender<RuntimeEvent>,
    SessionId,
    ClientId,
    PaneId,
    PaneId,
    PtySize,
) {
    let (mut rt, fake, tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let pane_a = other_pane(&rt, sid, root);
    let size_a = rt.pty_sizes[&pane_a];
    (rt, fake, tx, sid, client_id, root, pane_a, size_a)
}

#[test]
fn resize_pane_grows_the_focused_pane_and_reflows_its_pty() {
    let (mut rt, fake, _tx, _sid, client_id, _root, pane_a, size_a) = resize_fixture();

    // The client focuses A (the fresh split). Growing A's left border by 5
    // takes 5 columns from root; root has no PTY, so exactly one PtyResized
    // accompanies the LayoutChanged.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction: Direction::Left,
            size: 5,
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 2);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let expected = PtySize {
        cols: size_a.cols + 5,
        rows: size_a.rows,
    };
    assert_eq!(rt.pty_sizes[&pane_a], expected);
    assert_eq!(*fake.resizes(pane_a).unwrap().last().unwrap(), expected);
}

#[test]
fn resize_pane_negative_size_shrinks_the_focused_pane() {
    let (mut rt, fake, _tx, _sid, client_id, _root, pane_a, size_a) = resize_fixture();

    // The client focuses A. A negative size moves A's left border inward:
    // A gives 5 columns to root across that border.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction: Direction::Left,
            size: -5,
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 2);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let expected = PtySize {
        cols: size_a.cols - 5,
        rows: size_a.rows,
    };
    assert_eq!(rt.pty_sizes[&pane_a], expected);
    assert_eq!(*fake.resizes(pane_a).unwrap().last().unwrap(), expected);
}

#[test]
fn resize_pane_via_in_session_cli_defaults_to_the_issuing_pane() {
    let (mut rt, _fake, _tx, sid, client_id, root, pane_a, size_a) = resize_fixture();

    // Issued from inside root's pane with no explicit target: root grows
    // right by 3, so its neighbor A donates 3 columns.
    let source = CommandSource::in_session_cli(sid, Some(client_id), root, PathBuf::from("/sock"));
    let env = envelope_from(
        source,
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction: Direction::Right,
            size: 3,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    let expected = PtySize {
        cols: size_a.cols - 3,
        rows: size_a.rows,
    };
    assert_eq!(rt.pty_sizes[&pane_a], expected);
}

#[test]
fn resize_pane_explicit_target_resolves_its_owning_session() {
    let (mut rt, _fake, _tx, _sid, _client_id, _root, pane_a, size_a) = resize_fixture();

    // An internal source carries no session or client context; the explicit
    // pane target alone finds the owning session.
    let env = envelope(Command::ResizePane(ResizePaneArgs {
        pane: Some(pane_a),
        direction: Direction::Left,
        size: 2,
    }));
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    let expected = PtySize {
        cols: size_a.cols + 2,
        rows: size_a.rows,
    };
    assert_eq!(rt.pty_sizes[&pane_a], expected);
}

#[test]
fn resize_pane_min_size_rejection_reports_the_spare_and_mutates_nothing() {
    let (mut rt, fake, _tx, sid, client_id, _root, pane_a, size_a) = resize_fixture();
    let viewport = Size { cols: 80, rows: 24 };
    let rects_before = Server::tab_content_rects(
        &rt.sessions[&sid],
        rt.sessions[&sid].tabs.keys().copied().next().unwrap(),
        viewport,
        MIN_PANE_SIZE,
    );
    let resizes_before = fake.resizes(pane_a).unwrap().len();

    // At 80 columns the donor root holds 40; its border-inclusive floor is 4
    // (the 2-column content minimum plus the 1-cell border on each side), so
    // it can give exactly 36. Asking for 100 rejects and leaves everything
    // untouched.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction: Direction::Left,
            size: 100,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("the donating pane has only 36 spare cells to give".to_string()),
        }
    );

    let rects_after = Server::tab_content_rects(
        &rt.sessions[&sid],
        rt.sessions[&sid].tabs.keys().copied().next().unwrap(),
        viewport,
        MIN_PANE_SIZE,
    );
    assert_eq!(rects_after, rects_before);
    assert_eq!(fake.resizes(pane_a).unwrap().len(), resizes_before);
    assert_eq!(rt.pty_sizes[&pane_a], size_a);
}

#[test]
fn resize_pane_at_the_tab_edge_moves_the_opposite_border_instead() {
    let (mut rt, _fake, _tx, _sid, client_id, _root, pane_a, size_a) = resize_fixture();

    // A touches the tab's right edge: no right border exists, so the left
    // border moves right instead — A shrinks by the cell and the left
    // sibling gains it.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction: Direction::Right,
            size: 1,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let expected = PtySize {
        cols: size_a.cols - 1,
        rows: size_a.rows,
    };
    assert_eq!(rt.pty_sizes[&pane_a], expected);
}

#[test]
fn resize_pane_negative_size_at_the_edge_grows_via_the_opposite_border() {
    let (mut rt, _fake, _tx, _sid, client_id, _root, pane_a, size_a) = resize_fixture();

    // A negative size toward the tab edge (shrink away from a border that
    // does not exist) falls back the same way: the opposite border moves in
    // the same visual direction, so A grows by the cell.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction: Direction::Right,
            size: -1,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let expected = PtySize {
        cols: size_a.cols + 1,
        rows: size_a.rows,
    };
    assert_eq!(rt.pty_sizes[&pane_a], expected);
}

#[test]
fn resize_pane_with_no_border_on_the_axis_is_rejected() {
    let (mut rt, _fake, _tx, _sid, client_id, _root, pane_a, size_a) = resize_fixture();

    // The layout has no vertical split level at all: Up finds no border and
    // neither does the opposite-side fallback, so the resize rejects whole.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction: Direction::Up,
            size: 1,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane has no border to move on that axis".to_string()),
        }
    );
    assert_eq!(rt.pty_sizes[&pane_a], size_a);
}

#[test]
fn resize_pane_size_zero_is_rejected() {
    let (mut rt, _fake, _tx, _sid, client_id, _root, pane_a, size_a) = resize_fixture();

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction: Direction::Left,
            size: 0,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("resize size must be non-zero".to_string()),
        }
    );
    assert_eq!(rt.pty_sizes[&pane_a], size_a);
}

#[test]
fn resize_pane_with_no_attached_client_is_rejected() {
    let (mut rt, _tx) = new_runtime();
    let tab = TabId::new();
    let pane_left = PaneId::new();
    let pane_right = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_left);
    add_pane(&mut session, pane_right);
    add_tab(&mut session, tab, pane_left);
    session
        .tabs
        .get_mut(&tab)
        .unwrap()
        .update_layout(side_by_side(pane_left, pane_right));
    let sid = session.id;
    rt.sessions.insert(sid, session);
    let viewport = Size { cols: 80, rows: 24 };
    let rects_before = Server::tab_content_rects(&rt.sessions[&sid], tab, viewport, MIN_PANE_SIZE);

    // No client is attached anywhere, so no tab is viewed and no terminal
    // displays the result.
    let env = envelope(Command::ResizePane(ResizePaneArgs {
        pane: Some(pane_left),
        direction: Direction::Right,
        size: 1,
    }));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane's tab is not viewed by any client".to_string()),
        }
    );
    assert_eq!(
        Server::tab_content_rects(&rt.sessions[&sid], tab, viewport, MIN_PANE_SIZE),
        rects_before
    );
}

#[test]
fn resize_pane_in_an_unviewed_tab_is_rejected() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_front = TabId::new();
    let tab_back = TabId::new();
    let root_front = PaneId::new();
    let pane_left = PaneId::new();
    let pane_right = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root_front);
    add_pane(&mut session, pane_left);
    add_pane(&mut session, pane_right);
    add_tab(&mut session, tab_front, root_front);
    add_tab(&mut session, tab_back, pane_left);
    session
        .tabs
        .get_mut(&tab_back)
        .unwrap()
        .update_layout(side_by_side(pane_left, pane_right));
    add_client(&mut session, client_id, tab_front, Some(root_front));
    let sid = session.id;
    rt.sessions.insert(sid, session);
    let viewport = Size { cols: 80, rows: 24 };
    let rects_before =
        Server::tab_content_rects(&rt.sessions[&sid], tab_back, viewport, MIN_PANE_SIZE);

    // A client is attached, but none views the back tab — no terminal
    // displays the result, so the resize rejects and mutates nothing.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane: Some(pane_left),
            direction: Direction::Right,
            size: 4,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane's tab is not viewed by any client".to_string()),
        }
    );
    assert_eq!(
        Server::tab_content_rects(&rt.sessions[&sid], tab_back, viewport, MIN_PANE_SIZE),
        rects_before
    );
}

#[test]
fn resize_pane_in_a_nested_split_moves_the_enclosing_border() {
    let (mut rt, _fake, _tx, sid, client_id, root, pane_a, _size_a) = resize_fixture();

    // Split root again: [[root | B] | A]. B touches the inner split's right
    // edge, so growing B rightward moves the OUTER border — the whole inner
    // split takes 4 columns from A, and B's own share grows by 2.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source: Some(root),
            ..NewPaneArgs::default()
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let pane_b = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != root && *id != pane_a)
        .expect("a third pane exists");
    let size_a = rt.pty_sizes[&pane_a];
    let size_b = rt.pty_sizes[&pane_b];

    // The client's focus followed the fresh split to B.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction: Direction::Right,
            size: 4,
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // LayoutChanged + PtyResized for A and B (root has no PTY).
            assert_eq!(emitted_events.len(), 3);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(
        rt.pty_sizes[&pane_a],
        PtySize {
            cols: size_a.cols - 4,
            rows: size_a.rows,
        }
    );
    assert_eq!(
        rt.pty_sizes[&pane_b],
        PtySize {
            cols: size_b.cols + 2,
            rows: size_b.rows,
        }
    );
}

// --- NewTab handler ----------------------------------------------------------

#[test]
fn new_tab_spawns_creates_and_focuses_for_the_issuer() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // TabCreated, PaneCreated, TabFocused, PaneFocused, PtyResized;
            // the vacated tab has no viewer left, so nothing else reflows.
            assert_eq!(emitted_events.len(), 5);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let session = &rt.sessions[&sid];
    assert_eq!(session.tabs.len(), 2);
    let new_tab = session
        .tabs
        .values()
        .find(|tab| tab.id() != tab_a)
        .expect("the created tab");
    assert!(
        new_tab.name().starts_with("T-"),
        "generated tab name, got {}",
        new_tab.name()
    );
    assert_eq!(new_tab.index(), 1);
    let new_pane = new_tab.layout().leaf_panes()[0];

    // The issuer switched onto the new tab and focuses its root pane.
    let client = session.clients.get(client_id).unwrap();
    assert_eq!(client.active_tab(), new_tab.id());
    assert_eq!(client.focused_pane(new_tab.id()), Some(new_pane));

    // Root pane runs default shell in 80x22 middle region -> 78x20 content.
    let record = session.panes.get(new_pane).unwrap();
    assert_eq!(*record.lifecycle(), PaneLifecycle::Running);
    assert_eq!(record.command, None);
    assert_eq!(record.title, None);
    assert!(rt.pty_handles.contains_key(&new_pane));
    assert_eq!(
        fake.resizes(new_pane).unwrap(),
        vec![PtySize { cols: 78, rows: 20 }]
    );
    assert_eq!(rt.pty_sizes[&new_pane], PtySize { cols: 78, rows: 20 });
}

#[test]
fn new_tab_root_pane_carries_the_in_session_identity_env() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    ));

    // The root pane's spec is the default shell plus the identity vars naming
    // this session, the issuing client, and the root pane itself.
    let session = &rt.sessions[&sid];
    let new_tab = session
        .tabs
        .values()
        .find(|tab| tab.id() != tab_a)
        .expect("the created tab");
    let new_pane = new_tab.layout().leaf_panes()[0];
    let mut expected = rt.default_shell_spec(None, BTreeMap::new());
    expected.env.extend(koshi_env(
        sid,
        Some(client_id),
        new_pane,
        koshi_paths::runtime_dir().as_deref(),
    ));
    assert_eq!(fake.spawn_spec(new_pane).unwrap(), expected);
}

#[test]
fn new_tab_generates_a_free_name() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    ));
    assert!(matches!(result, CommandResult::Ok { .. }));

    // The generated name is `T-<adjective>-<noun>` with both words drawn from
    // the same language's lists, and does not collide with the existing tab.
    let session = &rt.sessions[&sid];
    let new_tab = session
        .tabs
        .values()
        .find(|tab| tab.id() != tab_a)
        .expect("the created tab");
    let mut pieces = new_tab.name().splitn(3, '-');
    assert_eq!(pieces.next(), Some("T"));
    let adjective = pieces.next().expect("adjective");
    let noun = pieces.next().expect("noun");
    let language_pairs = [
        (&naming::EN_ADJECTIVES, &naming::EN_NOUNS),
        (&naming::JA_ADJECTIVES, &naming::JA_NOUNS),
        (&naming::ZH_HANT_ADJECTIVES, &naming::ZH_HANT_NOUNS),
    ];
    let language = language_pairs
        .iter()
        .position(|(adjectives, _)| adjectives.contains(&adjective))
        .expect("adjective from a known language list");
    assert!(language_pairs[language].1.contains(&noun));
    assert_ne!(new_tab.name(), "t");
}

#[test]
fn new_tab_spawn_failure_commits_nothing() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    fake.fail_spawns_with(PtyError::Spawn {
        detail: "boom".to_string(),
    });
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("failed to launch the pane's process".to_string()),
        }
    );

    // Nothing was committed: no tab, no pane record, no view moved, no handle.
    let session = &rt.sessions[&sid];
    assert_eq!(session.tabs.len(), 1);
    assert_eq!(session.panes.len(), 1);
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), tab_a);
    assert!(rt.pty_handles.is_empty());
}

#[test]
fn new_tab_explicit_client_wins_over_the_issuer() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let issuer = ClientId::new();
    let named = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, issuer, tab_a, Some(pane_a));
    add_client(&mut session, named, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(issuer),
        Command::NewTab(NewTabArgs {
            client: Some(named),
            ..NewTabArgs::default()
        }),
    ));
    assert!(matches!(result, CommandResult::Ok { .. }));

    let session = &rt.sessions[&sid];
    let new_tab_id = session
        .tabs
        .values()
        .find(|tab| tab.id() != tab_a)
        .expect("the created tab")
        .id();
    assert_eq!(session.clients.get(named).unwrap().active_tab(), new_tab_id);
    assert_eq!(session.clients.get(issuer).unwrap().active_tab(), tab_a);
}

#[test]
fn new_tab_with_an_unattached_explicit_client_is_rejected() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewTab(NewTabArgs {
            client: Some(ClientId::new()),
            ..NewTabArgs::default()
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    assert_eq!(rt.sessions[&sid].tabs.len(), 1);
    assert!(fake.spawned_panes().is_empty());
}

#[test]
fn new_tab_external_source_defaults_to_the_sole_client() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let result = rt.dispatch(envelope_from(
        CommandSource::ExternalCli {
            session_id: Some(sid),
        },
        Command::NewTab(NewTabArgs::default()),
    ));
    assert!(matches!(result, CommandResult::Ok { .. }));

    let session = &rt.sessions[&sid];
    let new_tab_id = session
        .tabs
        .values()
        .find(|tab| tab.id() != tab_a)
        .expect("the created tab")
        .id();
    assert_eq!(
        session.clients.get(client_id).unwrap().active_tab(),
        new_tab_id
    );
}

#[test]
fn new_tab_external_source_with_two_clients_is_ambiguous() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, ClientId::new(), tab_a, None);
    add_client(&mut session, ClientId::new(), tab_a, None);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::ExternalCli {
            session_id: Some(sid),
        },
        Command::NewTab(NewTabArgs::default()),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some("several clients are attached; name the target client".to_string()),
        }
    );
    assert!(fake.spawned_panes().is_empty());
}

#[test]
fn new_tab_with_no_attached_client_is_stale() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::ExternalCli {
            session_id: Some(sid),
        },
        Command::NewTab(NewTabArgs::default()),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: Some("no client is attached to the session".to_string()),
        }
    );
    assert!(fake.spawned_panes().is_empty());
}

#[test]
fn new_tab_reflows_the_vacated_tab_for_its_remaining_viewer() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);

    // Mover A is the 40x10 size constraint on the shared tab; B views it at
    // the full 80x24.
    let mover = ClientId::new();
    let mut a = Client::new(
        mover,
        session.id,
        SystemTime::now(),
        Size { cols: 40, rows: 10 },
        tab_a,
    );
    a.update_focused_pane(tab_a, pane_a);
    session.attach_client(a);
    let stayer = ClientId::new();
    add_client(&mut session, stayer, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Split the shared tab so it holds a live PTY sized to the 40x10
    // constraint: chrome leaves 40x8; half-columns yield 18x6 content.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(stayer),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_x = other_pane(&rt, sid, pane_a);
    assert_eq!(
        *fake.resizes(pane_x).unwrap().last().unwrap(),
        PtySize { cols: 18, rows: 6 }
    );

    // A creates a new tab and leaves: vacated pane region grows to 80x22,
    // giving 38x20 content per half. A's new 40x8 pane region gives 38x6.
    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(mover),
        Command::NewTab(NewTabArgs::default()),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => {
            // TabCreated, PaneCreated, TabFocused, PaneFocused, PtyResized
            // (spawn), then the vacated tab's one live PTY reflowed
            // (`pane_a` never spawned, so only the split pane resizes).
            assert_eq!(emitted_events.len(), 6);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        *fake.resizes(pane_x).unwrap().last().unwrap(),
        PtySize { cols: 38, rows: 20 }
    );
    let new_tab = rt.sessions[&sid]
        .tabs
        .values()
        .find(|tab| tab.id() != tab_a)
        .expect("the created tab");
    let new_pane = new_tab.layout().leaf_panes()[0];
    assert_eq!(
        fake.resizes(new_pane).unwrap(),
        vec![PtySize { cols: 38, rows: 6 }]
    );
}

// --- CloseTab handler ----------------------------------------------------------

#[test]
fn close_tab_removes_state_kills_children_and_moves_viewers() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, client_id, tab_b, Some(pane_b));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Give the doomed tab a live PTY by splitting it while viewed.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_x = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_a && *id != pane_b)
        .expect("the split pane");

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab: Some(tab_b),
            force: false,
            tree: false,
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // PaneClosing+PaneRemoved for each of the two panes, TabClosed,
            // TabFocused (the viewer moves to tab_a; its pane has no PTY, so
            // nothing reflows).
            assert_eq!(emitted_events.len(), 6);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let session = &rt.sessions[&sid];
    assert!(!session.tabs.contains_key(&tab_b));
    assert!(session.panes.get(pane_b).is_none());
    assert!(session.panes.get(pane_x).is_none());
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), tab_a);
    assert!(!rt.pty_handles.contains_key(&pane_x));
    assert!(!rt.pty_sizes.contains_key(&pane_x));
    assert_eq!(
        wait_for_kill(&fake, pane_x),
        vec![KillPolicy::Graceful {
            timeout: GRACEFUL_TIMEOUT_DURATION
        }]
    );
}

/// A backend whose `kill` blocks on a shared barrier before delegating to the
/// wrapped fake. Every expected kill must have started before any completes,
/// so the test distinguishes concurrent per-pane kill threads from a serial
/// kill loop: serial, the first kill waits on the barrier forever and the
/// later kills never start.
struct BarrierKillBackend {
    inner: Arc<FakePtyBackend>,
    barrier: Barrier,
}

impl PtyBackend for BarrierKillBackend {
    fn spawn(
        &self,
        pane_id: PaneId,
        spec: SpawnSpec,
        size: PtySize,
    ) -> Result<PtyHandle, PtyError> {
        self.inner.spawn(pane_id, spec, size)
    }
    fn resize(&self, pane: PaneId, size: PtySize) -> Result<(), PtyError> {
        self.inner.resize(pane, size)
    }
    fn write(&self, pane: PaneId, bytes: &[u8]) -> Result<(), PtyError> {
        self.inner.write(pane, bytes)
    }
    fn kill(&self, pane: PaneId, kill_policy: KillPolicy) -> Result<(), PtyError> {
        self.barrier.wait();
        self.inner.kill(pane, kill_policy)
    }
}

#[test]
fn close_tab_kills_every_pane_concurrently() {
    // The doomed tab holds three panes (the PTY-less root plus two spawned
    // splits); the barrier releases a kill only once all three have started.
    let fake = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(BarrierKillBackend {
        inner: fake.clone(),
        barrier: Barrier::new(3),
    });
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel();
    let mut rt = Server::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx.clone(),
        Direction::Right,
    );

    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, client_id, tab_b, Some(pane_b));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Two splits give the doomed tab two live PTYs.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let spawned: Vec<PaneId> = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .filter(|id| *id != pane_a && *id != pane_b)
        .collect();
    let [pane_x, pane_y] = spawned[..] else {
        panic!("expected exactly two split panes, got {spawned:?}");
    };

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab: Some(tab_b),
            force: false,
            tree: false,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    // Both live children die with their own graceful policy; reaching this at
    // all proves the kills started concurrently, or the barrier would have
    // held the lone serial kill thread forever.
    assert_eq!(
        wait_for_kill(&fake, pane_x),
        vec![KillPolicy::Graceful {
            timeout: GRACEFUL_TIMEOUT_DURATION
        }]
    );
    assert_eq!(
        wait_for_kill(&fake, pane_y),
        vec![KillPolicy::Graceful {
            timeout: GRACEFUL_TIMEOUT_DURATION
        }]
    );
}

#[test]
fn close_tab_with_a_busy_confirm_pane_rejects_without_force() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, client_id, tab_b, Some(pane_b));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_x = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_a && *id != pane_b)
        .expect("the split pane");
    rt.sessions
        .get_mut(&sid)
        .unwrap()
        .panes
        .get_mut(pane_x)
        .unwrap()
        .close_policy = PaneClosePolicy::ConfirmIfBusy;

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab: Some(tab_b),
            force: false,
            tree: false,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("a pane in the tab may be busy; pass --force to close anyway".to_string()),
        }
    );

    // All-or-nothing: nothing was closed, nothing killed.
    let session = &rt.sessions[&sid];
    assert!(session.tabs.contains_key(&tab_b));
    assert!(session.panes.get(pane_x).is_some());
    assert!(rt.pty_handles.contains_key(&pane_x));
    assert!(fake.kills(pane_x).unwrap().is_empty());
}

#[test]
fn close_tab_force_kills_a_busy_confirm_pane() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, client_id, tab_b, Some(pane_b));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_x = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_a && *id != pane_b)
        .expect("the split pane");
    rt.sessions
        .get_mut(&sid)
        .unwrap()
        .panes
        .get_mut(pane_x)
        .unwrap()
        .close_policy = PaneClosePolicy::ConfirmIfBusy;

    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab: Some(tab_b),
            force: true,
            tree: false,
        }),
    ));
    assert!(matches!(result, CommandResult::Ok { .. }));
    assert!(!rt.sessions[&sid].tabs.contains_key(&tab_b));
    assert_eq!(wait_for_kill(&fake, pane_x), vec![KillPolicy::Force]);
}

#[test]
fn close_tab_confirm_if_busy_exited_pane_closes() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, client_id, tab_b, Some(pane_b));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_x = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_a && *id != pane_b)
        .expect("the split pane");
    {
        let record = rt
            .sessions
            .get_mut(&sid)
            .unwrap()
            .panes
            .get_mut(pane_x)
            .unwrap();
        record.close_policy = PaneClosePolicy::ConfirmIfBusy;
        record
            .update_lifecycle(PaneLifecycleEvent::ProcessExited {
                code: Some(0),
                at: SystemTime::now(),
            })
            .unwrap();
    }

    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab: Some(tab_b),
            force: false,
            tree: false,
        }),
    ));
    assert!(matches!(result, CommandResult::Ok { .. }));
    assert!(!rt.sessions[&sid].tabs.contains_key(&tab_b));
    assert_eq!(
        wait_for_kill(&fake, pane_x),
        vec![KillPolicy::Graceful {
            timeout: GRACEFUL_TIMEOUT_DURATION
        }]
    );
}

#[test]
fn close_last_tab_quits_the_session() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::CloseTab(CloseTabArgs::default()),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // PaneClosing, PaneRemoved, TabClosed, Quit.
            assert_eq!(emitted_events.len(), 4);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &rt.sessions[&sid];
    assert!(session.tabs.is_empty());
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn close_tab_with_an_unknown_explicit_tab_is_not_found() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab: Some(TabId::new()),
            force: false,
            tree: false,
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
    assert_eq!(rt.sessions[&sid].tabs.len(), 1);
}

#[test]
fn close_tab_reflows_the_tab_its_viewers_move_to() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);

    // B (80x24) views tab_a; the mover A (40x10) views the doomed tab_b.
    let stayer = ClientId::new();
    add_client(&mut session, stayer, tab_a, Some(pane_a));
    let mover = ClientId::new();
    let a = Client::new(
        mover,
        session.id,
        SystemTime::now(),
        Size { cols: 40, rows: 10 },
        tab_b,
    );
    session.attach_client(a);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Split tab_a while only B views it: 80x24 leaves an 80x22 pane region,
    // so each half's content is 38x20.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(stayer),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_x = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_a && *id != pane_b)
        .expect("the split pane");
    assert_eq!(
        *fake.resizes(pane_x).unwrap().last().unwrap(),
        PtySize { cols: 38, rows: 20 }
    );

    // A closes its tab and joins tab_a: 40x10 leaves a 40x8 pane region,
    // so each half's content is 18x6.
    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(mover),
        Command::CloseTab(CloseTabArgs::default()),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => {
            // PaneClosing, PaneRemoved, TabClosed, TabFocused, PtyResized.
            assert_eq!(emitted_events.len(), 5);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        *fake.resizes(pane_x).unwrap().last().unwrap(),
        PtySize { cols: 18, rows: 6 }
    );
    assert_eq!(
        rt.sessions[&sid].clients.get(mover).unwrap().active_tab(),
        tab_a
    );
}

// --- RenameTab handler ---------------------------------------------------------

#[test]
fn rename_tab_assigns_a_generated_name_and_emits() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // No explicit target: the issuer's active tab gets a fresh generated
    // name — the caller supplies none.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::RenameTab(RenameTabArgs { tab: None }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let name = rt.sessions[&sid].tabs[&tab_a].name().to_string();
    assert_ne!(name, "t");
    assert!(name.starts_with("T-"), "generated tab name, got {name}");
}

#[test]
fn rename_tab_explicit_tab_gets_a_generated_name() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a); // add_tab names it "t"
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::RenameTab(RenameTabArgs { tab: Some(tab_a) }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 1),
        other => panic!("expected Ok, got {other:?}"),
    }
    let name = rt.sessions[&sid].tabs[&tab_a].name().to_string();
    assert_ne!(name, "t");
    assert!(name.starts_with("T-"), "generated tab name, got {name}");
}

#[test]
fn move_tab_reorders_and_emits() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let tab_c = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let pane_c = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_pane(&mut session, pane_c);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_tab(&mut session, tab_c, pane_c);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Explicit tab C (slot 2) to the front.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::MoveTab(MoveTabArgs {
            tab: Some(tab_c),
            index: 0,
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    // New order C, A, B — the others closed ranks behind the moved tab.
    let session = &rt.sessions[&sid];
    assert_eq!(session.tabs[&tab_c].index(), 0);
    assert_eq!(session.tabs[&tab_a].index(), 1);
    assert_eq!(session.tabs[&tab_b].index(), 2);
    // Order-only change: the client still views the same tab.
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), tab_a);
}

#[test]
fn move_tab_defaults_to_the_issuers_active_tab() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    // The issuer views tab B (slot 1).
    add_client(&mut session, client_id, tab_b, Some(pane_b));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::MoveTab(MoveTabArgs {
            tab: None,
            index: 0,
        }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 1),
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &rt.sessions[&sid];
    assert_eq!(session.tabs[&tab_b].index(), 0);
    assert_eq!(session.tabs[&tab_a].index(), 1);
}

#[test]
fn move_tab_clamps_an_out_of_range_index() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let tab_c = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let pane_c = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_pane(&mut session, pane_c);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_tab(&mut session, tab_c, pane_c);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Index 99 clamps to the last slot (2).
    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::MoveTab(MoveTabArgs {
            tab: Some(tab_a),
            index: 99,
        }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 1),
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &rt.sessions[&sid];
    assert_eq!(session.tabs[&tab_b].index(), 0);
    assert_eq!(session.tabs[&tab_c].index(), 1);
    assert_eq!(session.tabs[&tab_a].index(), 2);
}

#[test]
fn move_tab_to_its_current_slot_is_ok_with_no_events() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::MoveTab(MoveTabArgs {
            tab: Some(tab_a),
            index: 0,
        }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert!(emitted_events.is_empty()),
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &rt.sessions[&sid];
    assert_eq!(session.tabs[&tab_a].index(), 0);
    assert_eq!(session.tabs[&tab_b].index(), 1);
}

#[test]
fn in_session_cli_move_tab_defaults_to_the_source_pane_tab() {
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

    // MoveTab with no explicit tab — InSessionCli resolves via the tab
    // containing pane_a (tab A), not the client's active tab (tab B).
    let source =
        CommandSource::in_session_cli(session_id, Some(client_id), pane_a, PathBuf::from("/sock"));
    let result = rt.dispatch(envelope_from(
        source,
        Command::MoveTab(MoveTabArgs {
            tab: None,
            index: 1,
        }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 1),
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &rt.sessions[&session_id];
    assert_eq!(session.tabs[&tab_b].index(), 0);
    assert_eq!(session.tabs[&tab_a].index(), 1);
}

#[test]
fn move_tab_with_an_unknown_tab_is_rejected() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::MoveTab(MoveTabArgs {
            tab: Some(TabId::new()),
            index: 0,
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
    // A rejected move mutates nothing.
    assert_eq!(rt.sessions[&sid].tabs[&tab_a].index(), 0);
}

#[test]
fn rename_pane_assigns_a_generated_title_and_emits() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // No explicit target: the issuer's focused pane gets a fresh generated
    // title — the caller supplies none.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::RenamePane(RenamePaneArgs { pane: None }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let title = rt.sessions[&sid]
        .panes
        .get(pane_a)
        .expect("pane")
        .title
        .clone()
        .expect("generated title");
    assert!(title.starts_with("P-"), "generated pane title, got {title}");
}

#[test]
fn rename_pane_explicit_target_resolves_its_owning_session() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // An internal source carries no session or client context; the explicit
    // pane target alone finds the owning session.
    let env = envelope(Command::RenamePane(RenamePaneArgs { pane: Some(pane_a) }));
    match rt.dispatch(env) {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 1),
        other => panic!("expected Ok, got {other:?}"),
    }
    let title = rt.sessions[&sid]
        .panes
        .get(pane_a)
        .expect("pane")
        .title
        .clone()
        .expect("generated title");
    assert!(title.starts_with("P-"), "generated pane title, got {title}");
}

#[test]
fn rename_pane_in_session_cli_defaults_to_the_issuing_pane() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // No explicit target from the in-session CLI: the pane the command was
    // issued from is renamed.
    let source =
        CommandSource::in_session_cli(sid, Some(client_id), pane_a, PathBuf::from("/sock"));
    let result = rt.dispatch(envelope_from(
        source,
        Command::RenamePane(RenamePaneArgs { pane: None }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 1),
        other => panic!("expected Ok, got {other:?}"),
    }
    let title = rt.sessions[&sid]
        .panes
        .get(pane_a)
        .expect("pane")
        .title
        .clone()
        .expect("generated title");
    assert!(title.starts_with("P-"), "generated pane title, got {title}");
}

// --- RenameSession handler -------------------------------------------------

#[test]
fn rename_session_assigns_a_generated_name_and_emits() {
    let (mut rt, _tx) = new_runtime();
    let sid = SessionId::new();
    rt.sessions.insert(sid, bare_session(sid));

    // An explicit id names the target; the new name is generated, never
    // caller-supplied.
    let env = envelope(Command::RenameSession(RenameSessionArgs {
        session: Some(sid),
    }));
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let name = rt.sessions[&sid].name.clone();
    assert_ne!(name, "s");
    assert!(name.starts_with("S-"), "generated session name, got {name}");
}

#[test]
fn rename_session_in_session_cli_defaults_to_the_issuing_session() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);
    let other = SessionId::new();
    rt.sessions.insert(other, bare_session(other));

    // No explicit target from the in-session CLI: the session the command
    // was issued inside gets a fresh generated name, even with other
    // sessions around.
    let source =
        CommandSource::in_session_cli(sid, Some(client_id), pane_a, PathBuf::from("/sock"));
    let result = rt.dispatch(envelope_from(
        source,
        Command::RenameSession(RenameSessionArgs { session: None }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 1),
        other => panic!("expected Ok, got {other:?}"),
    }
    let name = rt.sessions[&sid].name.clone();
    assert_ne!(name, "s");
    assert!(name.starts_with("S-"), "generated session name, got {name}");
    assert_eq!(rt.sessions[&other].name, "s");
}

#[test]
fn rename_session_external_cli_defaults_to_the_envelope_session() {
    let (mut rt, _tx) = new_runtime();
    let sid = SessionId::new();
    rt.sessions.insert(sid, bare_session(sid));
    let other = SessionId::new();
    rt.sessions.insert(other, bare_session(other));

    // No explicit args target: the session the external CLI named on its
    // envelope gets a fresh generated name, even with other sessions around.
    let result = rt.dispatch(envelope_from(
        CommandSource::external_cli(Some(sid)),
        Command::RenameSession(RenameSessionArgs { session: None }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 1),
        other => panic!("expected Ok, got {other:?}"),
    }
    let name = rt.sessions[&sid].name.clone();
    assert_ne!(name, "s");
    assert!(name.starts_with("S-"), "generated session name, got {name}");
    assert_eq!(rt.sessions[&other].name, "s");
}

#[test]
fn rename_session_external_cli_without_a_session_context_is_rejected() {
    let (mut rt, _tx) = new_runtime();
    let sid = SessionId::new();
    rt.sessions.insert(sid, bare_session(sid));

    let env = envelope_from(
        CommandSource::external_cli(None),
        Command::RenameSession(RenameSessionArgs { session: None }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("name a target session".to_string()),
        }
    );
    assert_eq!(rt.sessions[&sid].name, "s");
}

#[test]
fn rename_session_without_an_id_from_outside_a_session_is_rejected() {
    let (mut rt, _tx) = new_runtime();
    let sid = SessionId::new();
    rt.sessions.insert(sid, bare_session(sid));

    // Even with a sole session to guess, an internal source must name its
    // target: the no-id flow is the in-session CLI's only.
    let env = envelope(Command::RenameSession(RenameSessionArgs { session: None }));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("name a target session".to_string()),
        }
    );
    assert_eq!(rt.sessions[&sid].name, "s");
}

#[test]
fn rename_session_keybinding_defaults_to_the_issuers_session() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // No explicit target from a keybinding: the session the pressing client
    // is attached to gets a fresh generated name.
    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::RenameSession(RenameSessionArgs { session: None }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 1),
        other => panic!("expected Ok, got {other:?}"),
    }
    let name = rt.sessions[&sid].name.clone();
    assert_ne!(name, "s");
    assert!(name.starts_with("S-"), "generated session name, got {name}");
}

#[test]
fn rename_session_without_an_id_from_a_mouse_source_is_rejected() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::mouse(client_id),
        Command::RenameSession(RenameSessionArgs { session: None }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("name a target session".to_string()),
        }
    );
    assert_eq!(rt.sessions[&sid].name, "s");
}

#[test]
fn rename_session_with_an_unknown_id_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let sid = SessionId::new();
    rt.sessions.insert(sid, bare_session(sid));

    let env = envelope(Command::RenameSession(RenameSessionArgs {
        session: Some(SessionId::new()),
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
    assert_eq!(rt.sessions[&sid].name, "s");
}

#[test]
fn rename_session_on_a_stopping_session_is_rejected() {
    let (mut rt, _tx) = new_runtime();
    let sid = SessionId::new();
    rt.sessions.insert(sid, stopping_session(sid));

    // An explicit id resolves outside the acting-session admission check;
    // the resolver still gates it: a winding-down session takes no mutations.
    let env = envelope(Command::RenameSession(RenameSessionArgs {
        session: Some(sid),
    }));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("session is stopping".to_string()),
        }
    );
    assert_eq!(rt.sessions[&sid].name, "s");
}

#[test]
fn rename_session_in_session_cli_on_a_stopping_session_is_rejected() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = stopping_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // The issuer's own session is winding down: admission rejects the rename.
    let source =
        CommandSource::in_session_cli(sid, Some(client_id), pane_a, PathBuf::from("/sock"));
    let env = envelope_from(
        source,
        Command::RenameSession(RenameSessionArgs { session: None }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("session is stopping".to_string()),
        }
    );
    assert_eq!(rt.sessions[&sid].name, "s");
}

#[test]
fn rename_session_generated_name_skips_taken_session_names() {
    let (mut rt, _tx) = new_runtime();
    let sid = SessionId::new();
    rt.sessions.insert(sid, bare_session(sid));
    let other = SessionId::new();
    rt.sessions.insert(other, bare_session(other));

    // The generator treats every existing session name as taken, so the new
    // name collides with neither the sibling's nor the target's old one.
    let result = rt.dispatch(envelope(Command::RenameSession(RenameSessionArgs {
        session: Some(sid),
    })));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 1),
        other => panic!("expected Ok, got {other:?}"),
    }
    let name = rt.sessions[&sid].name.clone();
    assert_ne!(name, "s");
    assert_ne!(name, rt.sessions[&other].name);
}

// --- FocusTab handler ----------------------------------------------------------

#[test]
fn focus_tab_switches_the_view_and_emits() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(tab_b),
            client: None,
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // TabFocused only: neither tab holds a live PTY to reflow.
            assert_eq!(emitted_events.len(), 1);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .active_tab(),
        tab_b
    );
}

#[test]
fn focus_tab_index_next_and_prev_resolve_against_the_display_order() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a); // index 0
    add_tab(&mut session, tab_b, pane_b); // index 1
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);
    let active = |rt: &Server| {
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .active_tab()
    };

    // Next from tab_a (index 0) steps to tab_b (index 1).
    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Next,
            client: None,
        }),
    ));
    assert!(matches!(result, CommandResult::Ok { .. }));
    assert_eq!(active(&rt), tab_b);

    // Next from the last tab wraps to the first.
    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Next,
            client: None,
        }),
    ));
    assert!(matches!(result, CommandResult::Ok { .. }));
    assert_eq!(active(&rt), tab_a);

    // Prev from the first tab wraps to the last.
    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Prev,
            client: None,
        }),
    ));
    assert!(matches!(result, CommandResult::Ok { .. }));
    assert_eq!(active(&rt), tab_b);

    // An explicit index resolves the tab at that display position.
    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Index(0),
            client: None,
        }),
    ));
    assert!(matches!(result, CommandResult::Ok { .. }));
    assert_eq!(active(&rt), tab_a);
}

#[test]
fn focus_tab_on_the_already_active_tab_is_ok_with_no_events() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(tab_a),
            client: None,
        }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert!(emitted_events.is_empty()),
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .active_tab(),
        tab_a
    );
}

#[test]
fn focus_tab_with_an_unknown_id_or_index_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    for target in [TabTarget::Id(TabId::new()), TabTarget::Index(9)] {
        let env = envelope_from(
            CommandSource::key_binding(client_id),
            Command::FocusTab(FocusTabArgs {
                target,
                client: None,
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
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .active_tab(),
        tab_a
    );
}

#[test]
fn focus_tab_explicit_client_wins_over_the_issuer() {
    let (mut rt, _tx) = new_runtime();
    let issuer = ClientId::new();
    let named = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, issuer, tab_a, Some(pane_a));
    add_client(&mut session, named, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(issuer),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(tab_b),
            client: Some(named),
        }),
    ));
    assert!(matches!(result, CommandResult::Ok { .. }));

    let session = &rt.sessions[&sid];
    assert_eq!(session.clients.get(named).unwrap().active_tab(), tab_b);
    assert_eq!(session.clients.get(issuer).unwrap().active_tab(), tab_a);
}

#[test]
fn focus_tab_with_an_unattached_explicit_client_is_rejected() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(tab_b),
            client: Some(ClientId::new()),
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .active_tab(),
        tab_a
    );
}

#[test]
fn focus_tab_external_source_defaults_to_the_sole_client() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let result = rt.dispatch(envelope_from(
        CommandSource::ExternalCli {
            session_id: Some(sid),
        },
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(tab_b),
            client: None,
        }),
    ));
    assert!(matches!(result, CommandResult::Ok { .. }));
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .active_tab(),
        tab_b
    );
}

#[test]
fn focus_tab_external_source_with_two_clients_is_ambiguous() {
    let (mut rt, _tx) = new_runtime();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, ClientId::new(), tab_a, None);
    add_client(&mut session, ClientId::new(), tab_a, None);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::ExternalCli {
            session_id: Some(sid),
        },
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(tab_a),
            client: None,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some("several clients are attached; name the target client".to_string()),
        }
    );
}

#[test]
fn focus_tab_with_no_attached_client_is_stale() {
    let (mut rt, _tx) = new_runtime();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::ExternalCli {
            session_id: Some(sid),
        },
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(tab_a),
            client: None,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: Some("no client is attached to the session".to_string()),
        }
    );
}

#[test]
fn focus_tab_reflows_both_the_target_and_the_left_tab() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);

    // The mover A (30x8) starts on tab_a; B (40x10) views tab_b.
    let mover = ClientId::new();
    let a = Client::new(
        mover,
        session.id,
        SystemTime::now(),
        Size { cols: 30, rows: 8 },
        tab_a,
    );
    session.attach_client(a);
    let stayer = ClientId::new();
    let b = Client::new(
        stayer,
        session.id,
        SystemTime::now(),
        Size { cols: 40, rows: 10 },
        tab_b,
    );
    let mut b = b;
    b.update_focused_pane(tab_b, pane_b);
    session.attach_client(b);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Split tab_b while only B (40x10) views it: chrome leaves 40x8,
    // so the new half-column PTY content is 18x6.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(stayer),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_x = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_a && *id != pane_b)
        .expect("the split pane");
    assert_eq!(
        *fake.resizes(pane_x).unwrap().last().unwrap(),
        PtySize { cols: 18, rows: 6 }
    );

    // A switches onto tab_b: full viewport minimum is 30x8, leaving a 30x6
    // pane region; each half's content is 13x4.
    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(mover),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(tab_b),
            client: None,
        }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => {
            // TabFocused + the tightened PTY's resize (tab_a holds no PTY).
            assert_eq!(emitted_events.len(), 2);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        *fake.resizes(pane_x).unwrap().last().unwrap(),
        PtySize { cols: 13, rows: 4 }
    );

    // A switches back to tab_a: tab_b loses the 30x8 constraint and reflows
    // back to B's 40x10 geometry — the left tab reflowed.
    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(mover),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(tab_a),
            client: None,
        }),
    ));
    assert!(matches!(result, CommandResult::Ok { .. }));
    assert_eq!(
        *fake.resizes(pane_x).unwrap().last().unwrap(),
        PtySize { cols: 18, rows: 6 }
    );
}

#[test]
fn new_tab_for_a_client_below_minimum_size_is_rejected() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab_a, pane_a);
    // A 1x1 viewport cannot hold even one minimum-size pane.
    let client = Client::new(
        client_id,
        session.id,
        SystemTime::now(),
        Size { cols: 1, rows: 1 },
        tab_a,
    );
    session.attach_client(client);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("not enough space for a new tab".to_string()),
        }
    );
    assert_eq!(rt.sessions[&sid].tabs.len(), 1);
    assert!(fake.spawned_panes().is_empty());
}

/// The [`resize_fixture`] tab zoomed onto the split pane through dispatch:
/// mode `Fullscreen { focused: pane_a }`, its PTY resized to the full-tab
/// content rect (80x24 viewport -> 78x22).
fn fullscreen_fixture() -> (
    Server,
    Arc<FakePtyBackend>,
    mpsc::Sender<RuntimeEvent>,
    SessionId,
    ClientId,
    PaneId,
    PaneId,
    PtySize,
) {
    let (mut rt, fake, tx, sid, client_id, root, pane_a, size_a) = resize_fixture();
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::TogglePaneFullscreen,
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    (rt, fake, tx, sid, client_id, root, pane_a, size_a)
}

/// The id of the session's single tab.
fn only_tab(rt: &Server, sid: SessionId) -> TabId {
    rt.sessions[&sid].tabs.keys().copied().next().unwrap()
}

/// How `client_id` sees `tab` laid out. Zoom is per-client, so the mode is read
/// off the client — the tab holds only the tree, and two clients on one tab can
/// answer this differently.
fn mode_of(rt: &Server, sid: SessionId, client_id: ClientId, tab: TabId) -> LayoutMode {
    rt.sessions[&sid]
        .clients
        .get(client_id)
        .expect("client")
        .layout_mode(tab)
}

#[test]
fn toggle_fullscreen_promotes_the_focused_pane_and_reflows_its_pty() {
    let (mut rt, fake, _tx, sid, client_id, _root, pane_a, _size_a) = resize_fixture();
    let tab = only_tab(&rt, sid);
    let tree_before = rt.sessions[&sid].tabs[&tab].layout().clone();

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::TogglePaneFullscreen,
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // LayoutChanged plus the promoted pane's PtyResized; the hidden
            // root has no PTY and the focus was already on the pane.
            assert_eq!(emitted_events.len(), 2);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let session = &rt.sessions[&sid];
    assert_eq!(
        mode_of(&rt, sid, client_id, tab),
        LayoutMode::Fullscreen { focused: pane_a }
    );
    // The mode is a solve-time overlay: the tree itself is untouched.
    assert_eq!(*session.tabs[&tab].layout(), tree_before);
    let full = PtySize { cols: 78, rows: 20 };
    assert_eq!(rt.pty_sizes[&pane_a], full);
    assert_eq!(*fake.resizes(pane_a).unwrap().last().unwrap(), full);
}

#[test]
fn toggle_fullscreen_off_restores_the_exact_prior_layout_and_sizes() {
    let (mut rt, fake, _tx, sid, client_id, _root, pane_a, size_a) = fullscreen_fixture();
    let tab = only_tab(&rt, sid);
    let tree_before = rt.sessions[&sid].tabs[&tab].layout().clone();

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::TogglePaneFullscreen,
    );
    match rt.dispatch(env) {
        CommandResult::Ok { emitted_events, .. } => {
            // LayoutChanged plus the pane shrinking back to its tiled rect.
            assert_eq!(emitted_events.len(), 2);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(mode_of(&rt, sid, client_id, tab), LayoutMode::Tiled);
    let session = &rt.sessions[&sid];
    assert_eq!(*session.tabs[&tab].layout(), tree_before);
    assert_eq!(rt.pty_sizes[&pane_a], size_a);
    assert_eq!(*fake.resizes(pane_a).unwrap().last().unwrap(), size_a);
}

#[test]
fn toggle_fullscreen_from_the_issuing_pane_moves_the_acting_focus() {
    let (mut rt, _fake, _tx, sid, client_id, root, pane_a, size_a) = resize_fixture();
    let tab = only_tab(&rt, sid);

    // Issued from inside root's pane while the client's focus is on the
    // split pane: root is promoted and the focus follows it.
    let source = CommandSource::in_session_cli(sid, Some(client_id), root, PathBuf::from("/sock"));
    let env = envelope_from(source, Command::TogglePaneFullscreen);
    match rt.dispatch(env) {
        CommandResult::Ok { emitted_events, .. } => {
            // LayoutChanged plus PaneFocused: root has no PTY to resize, and
            // the hidden pane keeps its last size eventlessly.
            assert_eq!(emitted_events.len(), 2);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let session = &rt.sessions[&sid];
    assert_eq!(
        mode_of(&rt, sid, client_id, tab),
        LayoutMode::Fullscreen { focused: root }
    );
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab),
        Some(root)
    );
    assert_eq!(session.tabs[&tab].focus_mru().first(), Some(&root));
    assert_eq!(rt.pty_sizes[&pane_a], size_a);
}

#[test]
fn toggle_fullscreen_on_an_unviewed_tab_is_rejected() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Issued from inside the unviewed tab's pane: the toggle would resize
    // real PTYs against a viewport no client provides.
    let source =
        CommandSource::in_session_cli(sid, Some(client_id), pane_b, PathBuf::from("/sock"));
    let env = envelope_from(source, Command::TogglePaneFullscreen);
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane's tab is not viewed by any client".to_string()),
        }
    );
    assert_eq!(mode_of(&rt, sid, client_id, tab_b), LayoutMode::Tiled);
}

#[test]
fn toggle_fullscreen_below_the_pane_floor_is_rejected() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab_id, pane);
    // A 3x3 viewport is below the border-inclusive floor (4 columns), so
    // even the whole tab cannot show the pane's content minimum.
    let mut client = Client::new(
        client_id,
        session.id,
        SystemTime::now(),
        Size { cols: 3, rows: 3 },
        tab_id,
    );
    client.update_focused_pane(tab_id, pane);
    session.attach_client(client);
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::TogglePaneFullscreen,
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("not enough space to fullscreen the pane".to_string()),
        }
    );
    assert_eq!(mode_of(&rt, sid, client_id, tab_id), LayoutMode::Tiled);
}

#[test]
fn focus_pane_under_fullscreen_retargets_the_zoom() {
    let (mut rt, _fake, _tx, sid, client_id, root, pane_a, _size_a) = fullscreen_fixture();
    let tab = only_tab(&rt, sid);
    let full = PtySize { cols: 78, rows: 20 };

    // Root is hidden behind the fullscreen — focusing it swaps the zoom to
    // it instead of rejecting or dropping the mode.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(root),
            client: None,
        }),
    );
    match rt.dispatch(env) {
        CommandResult::Ok { emitted_events, .. } => {
            // LayoutChanged plus PaneFocused: root has no PTY, and the
            // newly hidden pane keeps its last size eventlessly.
            assert_eq!(emitted_events.len(), 2);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let session = &rt.sessions[&sid];
    assert_eq!(
        mode_of(&rt, sid, client_id, tab),
        LayoutMode::Fullscreen { focused: root }
    );
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab),
        Some(root)
    );
    assert_eq!(rt.pty_sizes[&pane_a], full);
}

#[test]
fn focus_pane_retargeting_back_skips_the_unchanged_pty() {
    let (mut rt, fake, _tx, sid, client_id, root, pane_a, _size_a) = fullscreen_fixture();
    let tab = only_tab(&rt, sid);
    let full = PtySize { cols: 78, rows: 20 };

    let focus = |pane: PaneId| {
        envelope_from(
            CommandSource::key_binding(client_id),
            Command::FocusPane(FocusPaneArgs {
                target: FocusTarget::Pane(pane),
                client: None,
            }),
        )
    };
    assert!(matches!(rt.dispatch(focus(root)), CommandResult::Ok { .. }));
    let resizes_before = fake.resizes(pane_a).unwrap().len();

    // Retargeting back gives the pane the same full-tab rect it last held,
    // so the reflow applies nothing.
    match rt.dispatch(focus(pane_a)) {
        CommandResult::Ok { emitted_events, .. } => {
            // LayoutChanged plus PaneFocused, no PtyResized.
            assert_eq!(emitted_events.len(), 2);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        mode_of(&rt, sid, client_id, tab),
        LayoutMode::Fullscreen { focused: pane_a }
    );
    assert_eq!(rt.pty_sizes[&pane_a], full);
    assert_eq!(fake.resizes(pane_a).unwrap().len(), resizes_before);
}

#[test]
fn focus_pane_on_the_promoted_pane_is_a_no_op() {
    let (mut rt, _fake, _tx, sid, client_id, _root, pane_a, _size_a) = fullscreen_fixture();
    let tab = only_tab(&rt, sid);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(pane_a),
            client: None,
        }),
    );
    match rt.dispatch(env) {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(emitted_events.len(), 0);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        mode_of(&rt, sid, client_id, tab),
        LayoutMode::Fullscreen { focused: pane_a }
    );
}

/// One client zooming a pane changes nothing for another client on the same tab.
/// A zooms its pane; B keeps its tiled view, its own focus, and its own pane on
/// screen. Zoom is a fact about a view, not about the tab.
#[test]
fn one_clients_zoom_leaves_another_clients_view_alone() {
    let (mut rt, _tx) = new_runtime();
    let client_a = ClientId::new();
    let client_b = ClientId::new();
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, a);
    add_pane(&mut session, b);
    add_tab(&mut session, tab_id, a);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(side_by_side(a, b));
    add_client(&mut session, client_a, tab_id, Some(a));
    add_client(&mut session, client_b, tab_id, Some(b));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Client A zooms the pane it has focused.
    let env = envelope_from(
        CommandSource::key_binding(client_a),
        Command::TogglePaneFullscreen,
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert_eq!(
        mode_of(&rt, sid, client_a, tab_id),
        LayoutMode::Fullscreen { focused: a },
        "the client that zoomed sees its pane filling the tab"
    );
    assert_eq!(
        mode_of(&rt, sid, client_b, tab_id),
        LayoutMode::Tiled,
        "the other client keeps its tiled view: another client's zoom is not its business"
    );
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_b)
            .unwrap()
            .focused_pane(tab_id),
        Some(b),
        "and keeps its own focus"
    );

    // B re-focuses the pane it already holds: nothing about its view changed, so
    // nothing at all happens.
    let env = envelope_from(
        CommandSource::key_binding(client_b),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(b),
            client: None,
        }),
    );
    match rt.dispatch(env) {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(emitted_events.len(), 0);
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(mode_of(&rt, sid, client_b, tab_id), LayoutMode::Tiled);
}

/// A pane's PTY has ONE size, but its viewers can disagree about its rect: one
/// client zooms it, another still shows it tiled in a corner. The child is given
/// the SMALLEST rect among the clients that draw it — the largest grid every one
/// of them can show in full — so nobody is ever handed a grid too big to fit and
/// has to crop.
///
/// Concretely: pane_a is 78x20 tiled. Client A zooms it; client B still views the
/// tab tiled. pane_a's child stays 78x20 — A sees it alone on screen, but at the
/// size B can still display. With B gone (the single-client test above), the same
/// zoom gives the child the whole tab.
#[test]
fn a_zoom_does_not_grow_a_pane_another_client_still_shows_tiled() {
    let (mut rt, _fake, _tx, sid, client_a, root, pane_a, size_a) = resize_fixture();
    let tab = only_tab(&rt, sid);

    // A second client views the same tab, tiled, focused on the other pane.
    let client_b = ClientId::new();
    add_client(
        rt.sessions.get_mut(&sid).expect("session"),
        client_b,
        tab,
        Some(root),
    );

    let env = envelope_from(
        CommandSource::key_binding(client_a),
        Command::TogglePaneFullscreen,
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert_eq!(
        mode_of(&rt, sid, client_a, tab),
        LayoutMode::Fullscreen { focused: pane_a }
    );
    assert_eq!(mode_of(&rt, sid, client_b, tab), LayoutMode::Tiled);
    assert_eq!(
        rt.pty_sizes[&pane_a], size_a,
        "client B still draws pane_a tiled, so its child keeps the size B can show"
    );
}

/// The tiled client leaving takes its claim on the pane's size with it: the pane
/// is now drawn only by the client that has it zoomed, so the child finally grows
/// to fill the tab. The size follows the viewers who are actually looking.
#[test]
fn a_zoomed_pane_grows_once_the_client_holding_it_tiled_detaches() {
    let (mut rt, _fake, _tx, sid, client_a, root, pane_a, size_a) = resize_fixture();
    let tab = only_tab(&rt, sid);

    let client_b = ClientId::new();
    add_client(
        rt.sessions.get_mut(&sid).expect("session"),
        client_b,
        tab,
        Some(root),
    );

    let env = envelope_from(
        CommandSource::key_binding(client_a),
        Command::TogglePaneFullscreen,
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(
        rt.pty_sizes[&pane_a], size_a,
        "while B still shows it tiled, the child keeps the tiled size"
    );

    // B detaches. A, still zoomed, is the only client drawing pane_a.
    rt.handle_client_detach(client_b);

    assert_eq!(
        rt.pty_sizes[&pane_a],
        PtySize { cols: 78, rows: 20 },
        "with no tiled viewer left, the zoom finally gives the child the whole tab"
    );
}

/// A zoomed client focusing another pane swaps what its zoom shows — the zoomed
/// view changes content and stays on, and the tab's tree is never rewritten.
#[test]
fn focusing_another_pane_while_zoomed_swaps_what_the_zoom_shows() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, a);
    add_pane(&mut session, b);
    add_tab(&mut session, tab_id, a);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(side_by_side(a, b));
    add_client(&mut session, client_id, tab_id, Some(a));
    let sid = session.id;
    rt.sessions.insert(sid, session);
    let tree_before = rt.sessions[&sid].tabs[&tab_id].layout().clone();

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::TogglePaneFullscreen,
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    // Focus the pane the zoom is hiding: the zoom follows the focus onto it.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(b),
            client: None,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert_eq!(
        mode_of(&rt, sid, client_id, tab_id),
        LayoutMode::Fullscreen { focused: b },
        "the zoom moved to the newly focused pane"
    );
    assert_eq!(
        *rt.sessions[&sid].tabs[&tab_id].layout(),
        tree_before,
        "a zoom is a solve-time overlay: the tree is untouched"
    );
}

#[test]
fn new_pane_drops_the_splitting_clients_zoom() {
    let (mut rt, _fake, _tx, sid, client_id, _root, pane_a, _size_a) = fullscreen_fixture();
    let tab = only_tab(&rt, sid);

    // Splitting the promoted pane: the tab returns to the tiled view and
    // both halves of the split are sized against it (root 40, the split
    // pair 20 each -> 18x20 content after chrome rows).
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert_eq!(mode_of(&rt, sid, client_id, tab), LayoutMode::Tiled);
    let session = &rt.sessions[&sid];
    let pane_b = session
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_a && rt.pty_sizes.contains_key(id))
        .expect("the new pane");
    let half = PtySize { cols: 18, rows: 20 };
    assert_eq!(rt.pty_sizes[&pane_a], half);
    assert_eq!(rt.pty_sizes[&pane_b], half);
}

#[test]
fn resize_pane_drops_the_resizing_clients_zoom() {
    let (mut rt, _fake, _tx, sid, client_id, _root, pane_a, size_a) = fullscreen_fixture();
    let tab = only_tab(&rt, sid);

    // The moved border must be visible: the resize lands in the tiled view.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction: Direction::Left,
            size: 5,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert_eq!(mode_of(&rt, sid, client_id, tab), LayoutMode::Tiled);
    let expected = PtySize {
        cols: size_a.cols + 5,
        rows: size_a.rows,
    };
    assert_eq!(rt.pty_sizes[&pane_a], expected);
}

#[test]
fn close_pane_drops_the_closing_clients_zoom() {
    let (mut rt, _fake, _tx, sid, client_id, root, pane_a, _size_a) = fullscreen_fixture();
    let tab = only_tab(&rt, sid);
    let full = PtySize { cols: 78, rows: 20 };

    // Closing the hidden root: the survivor already fills the tab, so its
    // PTY keeps the full-tab size it holds.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane: Some(root),
            force: true,
            tree: false,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert_eq!(mode_of(&rt, sid, client_id, tab), LayoutMode::Tiled);
    let session = &rt.sessions[&sid];
    assert_eq!(*session.tabs[&tab].layout(), LayoutNode::Pane(pane_a));
    assert_eq!(rt.pty_sizes[&pane_a], full);
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab),
        Some(pane_a)
    );
}

#[test]
fn close_pane_closing_the_promoted_pane_drops_the_fullscreen() {
    let (mut rt, _fake, _tx, sid, client_id, root, pane_a, _size_a) = fullscreen_fixture();
    let tab = only_tab(&rt, sid);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane: Some(pane_a),
            force: true,
            tree: false,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert_eq!(mode_of(&rt, sid, client_id, tab), LayoutMode::Tiled);
    let session = &rt.sessions[&sid];
    assert_eq!(*session.tabs[&tab].layout(), LayoutNode::Pane(root));
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab),
        Some(root)
    );
}

/// The (rows, cols) of `pane`'s terminal-engine grid.
fn engine_dimensions(rt: &Server, pane: PaneId) -> (u16, u16) {
    rt.terminal_engines()[&pane]
        .state()
        .active_grid()
        .dimensions()
}

#[test]
fn new_pane_installs_a_terminal_engine_at_spawn_size() {
    let (rt, _fake, _tx, _sid, _client_id, root, pane_a, size_a) = resize_fixture();

    assert!(rt.terminal_engines().contains_key(&pane_a));
    assert_eq!(
        engine_dimensions(&rt, pane_a),
        (size_a.rows, size_a.cols),
        "engine grid matches the spawned PTY size"
    );
    // The fixture's root pane never spawned a PTY, so it has no engine.
    assert!(!rt.terminal_engines().contains_key(&root));
}

#[test]
fn close_pane_removes_the_terminal_engine() {
    let (mut rt, _fake, _tx, _sid, client_id, _root, pane_a, _size_a) = resize_fixture();
    assert!(rt.terminal_engines().contains_key(&pane_a));

    // No explicit pane: the focused (split) pane closes and its engine goes
    // with its PTY bookkeeping.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert!(!rt.terminal_engines().contains_key(&pane_a));
}

#[test]
fn new_tab_installs_a_terminal_engine_at_spawn_size() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    let new_pane = other_pane(&rt, sid, root);
    assert!(rt.terminal_engines().contains_key(&new_pane));
    assert_eq!(
        engine_dimensions(&rt, new_pane),
        (rt.pty_sizes[&new_pane].rows, rt.pty_sizes[&new_pane].cols),
        "engine grid matches the spawned PTY size"
    );
}

#[test]
fn close_tab_removes_the_terminal_engines_of_its_panes() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // A second tab whose single pane spawned an engine.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, sid, root);
    assert!(rt.terminal_engines().contains_key(&new_pane));

    // Closing the client's active tab (the new one) drops its pane's engine.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::CloseTab(CloseTabArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert!(!rt.terminal_engines().contains_key(&new_pane));
}

#[test]
fn resize_pane_resizes_the_terminal_engine_with_its_pty() {
    let (mut rt, _fake, _tx, _sid, client_id, _root, pane_a, size_a) = resize_fixture();

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction: Direction::Left,
            size: 5,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    // The reflow resized A's PTY by 5 columns; its engine grid follows.
    let expected = PtySize {
        cols: size_a.cols + 5,
        rows: size_a.rows,
    };
    assert_eq!(rt.pty_sizes[&pane_a], expected);
    assert_eq!(
        engine_dimensions(&rt, pane_a),
        (expected.rows, expected.cols)
    );
}

#[test]
fn child_exit_close_on_exit_removes_the_pane_and_reaps_it() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, sid, root);

    // The split pane's `CloseOnExit` child dies.
    let events = rt.handle_child_exit(new_pane, ExitStatus::ExitCode(0), SystemTime::UNIX_EPOCH);

    // The exit is reported first, carrying the pane and its code.
    match events.first() {
        Some(Event::PaneProcessExited(exited)) => {
            assert_eq!(exited.pane_id, new_pane);
            assert_eq!(exited.exit_code, Some(0));
        }
        other => panic!("expected PaneProcessExited first, got {other:?}"),
    }

    // The pane is gone from state and from every runtime map.
    assert!(rt.sessions[&sid].panes.get(new_pane).is_none());
    assert!(!rt.pty_handles.contains_key(&new_pane));
    assert!(!rt.pty_sizes.contains_key(&new_pane));
    assert!(!rt.terminal_engines.contains_key(&new_pane));

    // The already-dead child is only reaped: the backend entry is released via
    // a single inline `Force` kill (the `exited` guard sends no signal).
    assert_eq!(fake.kills(new_pane).unwrap(), vec![KillPolicy::Force]);

    // The root survives and reclaims the whole tab.
    assert_eq!(
        rt.sessions[&sid].tabs[&tab].layout(),
        &LayoutNode::Pane(root)
    );
}

#[test]
fn child_exit_of_the_last_pane_closes_the_tab_and_quits() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let events = rt.handle_child_exit(root, ExitStatus::ExitCode(0), SystemTime::UNIX_EPOCH);

    // Removing the last pane closes the tab, and closing the last tab quits.
    assert!(rt.sessions[&sid].panes.get(root).is_none());
    assert!(rt.sessions[&sid].tabs.is_empty());
    assert!(events.iter().any(|event| matches!(event, Event::Quit)));
}

#[test]
fn child_exit_empties_a_tab_and_moves_the_viewer_to_a_sibling() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_pane(&mut session, pane_b);
    add_tab(&mut session, tab_a, pane_a);
    add_tab(&mut session, tab_b, pane_b);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // The sole pane of tab A exits: tab A closes, but tab B survives, so the
    // session does not quit and the viewer moves to tab B (which reflows).
    let events = rt.handle_child_exit(pane_a, ExitStatus::ExitCode(0), SystemTime::UNIX_EPOCH);

    assert!(rt.sessions[&sid].panes.get(pane_a).is_none());
    assert!(!rt.sessions[&sid].tabs.contains_key(&tab_a));
    assert!(rt.sessions[&sid].tabs.contains_key(&tab_b));
    assert!(!events.iter().any(|event| matches!(event, Event::Quit)));
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_id)
            .unwrap()
            .active_tab(),
        tab_b
    );
}

#[test]
fn child_exit_of_an_unknown_pane_is_dropped() {
    let (mut rt, _tx) = new_runtime();

    // No session owns the pane (closed while its exit waited in the inbox).
    let events = rt.handle_child_exit(
        PaneId::new(),
        ExitStatus::ExitCode(0),
        SystemTime::UNIX_EPOCH,
    );

    assert!(events.is_empty());
}

#[test]
fn child_exit_by_signal_reports_no_exit_code() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, sid, root);

    let events = rt.handle_child_exit(new_pane, ExitStatus::Signaled(9), SystemTime::UNIX_EPOCH);

    // A signal has no numeric code.
    match events.first() {
        Some(Event::PaneProcessExited(exited)) => assert_eq!(exited.exit_code, None),
        other => panic!("expected PaneProcessExited first, got {other:?}"),
    }
}

#[test]
fn child_exit_respawn_shell_keeps_the_pane_and_its_bookkeeping() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let new_pane = other_pane(&rt, sid, root);

    // Make the pane respawn on exit, with its child marked running.
    {
        let record = rt
            .sessions
            .get_mut(&sid)
            .unwrap()
            .panes
            .get_mut(new_pane)
            .unwrap();
        record.exit_policy = PaneExitPolicy::RespawnShell;
        let _ = record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);
    }

    let events = rt.handle_child_exit(new_pane, ExitStatus::ExitCode(0), SystemTime::UNIX_EPOCH);

    // Only the exit is reported — the pane is not removed.
    assert_eq!(events.len(), 1);
    match events.first() {
        Some(Event::PaneProcessExited(exited)) => {
            assert_eq!(exited.pane_id, new_pane);
            assert_eq!(exited.exit_code, Some(0));
        }
        other => panic!("expected PaneProcessExited, got {other:?}"),
    }
    assert!(rt.sessions[&sid].panes.get(new_pane).is_some());

    // The lifecycle advanced Running -> Exited -> Spawning: the respawn decision.
    assert_eq!(
        *rt.sessions[&sid].panes.get(new_pane).unwrap().lifecycle(),
        PaneLifecycle::Spawning
    );

    // Bookkeeping is kept and the pane is never killed — it lives on.
    assert!(rt.pty_handles.contains_key(&new_pane));
    assert!(rt.pty_sizes.contains_key(&new_pane));
    assert!(rt.terminal_engines.contains_key(&new_pane));
    assert!(fake.kills(new_pane).unwrap().is_empty());
}

/// The `(session, tab, pane)` of a runtime `bootstrap_local` built: exactly one
/// of each. Panics unless the runtime holds a single session with a single tab
/// and a single pane.
fn only_slot(rt: &Server) -> (SessionId, TabId, PaneId) {
    let session = rt.sessions.values().next().expect("exactly one session");
    let tab_id = *session.tabs.keys().next().expect("exactly one tab");
    let pane = session
        .panes
        .list()
        .map(PaneRecord::id)
        .next()
        .expect("exactly one pane");
    (session.id, tab_id, pane)
}

#[test]
fn client_attach_reflows_the_shared_tab_to_the_smaller_effective_size() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let big = Size { cols: 80, rows: 24 };
    let small = Size { cols: 40, rows: 24 };
    rt.bootstrap_local(SessionId::new(), big, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (sid, tab_id, pane) = only_slot(&rt);
    let resizes_before = fake.resizes(pane).expect("pane spawned").len();

    // A smaller second client attaches to the same tab: the effective size drops
    // to the per-axis minimum, so the live pane's PTY reflows down.
    let events = rt.handle_client_attach(sid, ClientId::new(), small, tab_id, SystemTime::now());

    let expected = size_root_pane(pane, pane_viewport(small), MIN_PANE_SIZE);
    assert_eq!(fake.resizes(pane).unwrap().len(), resizes_before + 1);
    assert_eq!(*fake.resizes(pane).unwrap().last().unwrap(), expected);
    assert_eq!(
        events,
        vec![Event::PtyResized(PtyResized {
            pane_id: pane,
            size: expected,
        })]
    );
}

#[test]
fn client_resize_updates_full_viewport_and_reflows_middle_pane_region() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let initial = Size { cols: 80, rows: 24 };
    let resized = Size {
        cols: 100,
        rows: 30,
    };
    let client = rt
        .bootstrap_local(SessionId::new(), initial, SystemTime::now())
        .expect("bootstrap");
    let (_sid, _tab, pane) = only_slot(&rt);

    let events = rt.handle_client_resize(client, resized);
    let expected = size_root_pane(pane, pane_viewport(resized), MIN_PANE_SIZE);

    assert_eq!(
        rt.session_for_client(client)
            .unwrap()
            .clients
            .get(client)
            .unwrap()
            .viewport(),
        resized
    );
    assert_eq!(*fake.resizes(pane).unwrap().last().unwrap(), expected);
    assert_eq!(
        events,
        vec![Event::PtyResized(PtyResized {
            pane_id: pane,
            size: expected,
        })]
    );
}

#[test]
fn client_attach_of_a_larger_client_leaves_the_tab_size_unchanged() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let small = Size { cols: 40, rows: 24 };
    let big = Size { cols: 80, rows: 24 };
    rt.bootstrap_local(SessionId::new(), small, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (sid, tab_id, pane) = only_slot(&rt);
    let resizes_before = fake.resizes(pane).expect("pane spawned").len();

    // The larger client cannot lower the per-axis minimum, so the effective size
    // stays 40x24: no reflow, no resize event.
    let events = rt.handle_client_attach(sid, ClientId::new(), big, tab_id, SystemTime::now());

    assert!(events.is_empty());
    assert_eq!(fake.resizes(pane).unwrap().len(), resizes_before);
}

#[test]
fn client_detach_reflows_the_shared_tab_back_to_the_remaining_viewport() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let big = Size { cols: 80, rows: 24 };
    let small = Size { cols: 40, rows: 24 };
    rt.bootstrap_local(SessionId::new(), big, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (sid, tab_id, pane) = only_slot(&rt);

    // Two clients view the tab; the smaller one holds it at 40x24.
    let small_client = ClientId::new();
    rt.handle_client_attach(sid, small_client, small, tab_id, SystemTime::now());
    let resizes_before = fake.resizes(pane).expect("pane spawned").len();

    // The smaller client leaves: only the 80x24 viewer remains, so the tab grows
    // back and the pane's PTY reflows up.
    let events = rt.handle_client_detach(small_client);

    let expected = size_root_pane(pane, pane_viewport(big), MIN_PANE_SIZE);
    assert_eq!(fake.resizes(pane).unwrap().len(), resizes_before + 1);
    assert_eq!(*fake.resizes(pane).unwrap().last().unwrap(), expected);
    assert_eq!(
        events,
        vec![Event::PtyResized(PtyResized {
            pane_id: pane,
            size: expected,
        })]
    );
}

#[test]
fn last_client_detach_keeps_pty_sizes_and_emits_no_resize() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let big = Size { cols: 80, rows: 24 };
    let client = rt
        .bootstrap_local(SessionId::new(), big, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (sid, _tab_id, pane) = only_slot(&rt);
    let resizes_before = fake.resizes(pane).expect("pane spawned").len();

    // The only viewer leaves: the tab has no viewport, so its PTY keeps its size
    // and no resize event is produced. The pane itself stays alive.
    let events = rt.handle_client_detach(client);

    assert!(events.is_empty());
    assert_eq!(fake.resizes(pane).unwrap().len(), resizes_before);
    assert!(rt.sessions[&sid].clients.get(client).is_none());
    assert!(rt.sessions[&sid].panes.get(pane).is_some());
}

#[test]
fn client_attach_to_an_unknown_session_is_dropped() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let events = rt.handle_client_attach(
        SessionId::new(),
        ClientId::new(),
        Size { cols: 80, rows: 24 },
        TabId::new(),
        SystemTime::now(),
    );
    assert!(events.is_empty());
}

#[test]
fn client_detach_of_an_unknown_client_is_dropped() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let events = rt.handle_client_detach(ClientId::new());
    assert!(events.is_empty());
}

#[test]
fn client_attach_to_an_unknown_tab_is_dropped() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let big = Size { cols: 80, rows: 24 };
    rt.bootstrap_local(SessionId::new(), big, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (sid, _tab_id, pane) = only_slot(&rt);
    let resizes_before = fake.resizes(pane).expect("pane spawned").len();

    // The named tab is not one this session holds: the client is not attached
    // and nothing reflows.
    let stranger = ClientId::new();
    let events = rt.handle_client_attach(sid, stranger, big, TabId::new(), SystemTime::now());

    assert!(events.is_empty());
    assert!(rt.sessions[&sid].clients.get(stranger).is_none());
    assert_eq!(fake.resizes(pane).unwrap().len(), resizes_before);
}

#[test]
fn client_reattach_onto_a_different_tab_reflows_the_tab_it_left() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let big = Size { cols: 80, rows: 24 };
    let small = Size { cols: 40, rows: 24 };
    let client_a = rt
        .bootstrap_local(SessionId::new(), big, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (sid, tab_1, pane_1) = only_slot(&rt);

    // A second live tab: `NewTab` moves client A onto it, leaving `tab_1` with
    // no viewer and `pane_1` at its bootstrap size.
    match rt.dispatch(envelope_from(
        CommandSource::key_binding(client_a),
        Command::NewTab(NewTabArgs::default()),
    )) {
        CommandResult::Ok { .. } => {}
        other => panic!("expected Ok, got {other:?}"),
    }
    let tab_2 = rt.sessions[&sid]
        .tabs
        .values()
        .find(|tab| tab.id() != tab_1)
        .expect("the created tab")
        .id();

    // Two clients view `tab_1`; the smaller one (C) constrains `pane_1` to 40x24.
    let client_b = ClientId::new();
    let client_c = ClientId::new();
    rt.handle_client_attach(sid, client_b, big, tab_1, SystemTime::now());
    rt.handle_client_attach(sid, client_c, small, tab_1, SystemTime::now());
    assert_eq!(
        *fake.resizes(pane_1).unwrap().last().unwrap(),
        size_root_pane(pane_1, pane_viewport(small), MIN_PANE_SIZE)
    );
    let resizes_before = fake.resizes(pane_1).unwrap().len();

    // C re-attaches onto `tab_2`: it leaves `tab_1`, where only the 80x24 client
    // B remains, so `pane_1` grows back — the tab the client left is reflowed.
    let events = rt.handle_client_attach(sid, client_c, big, tab_2, SystemTime::now());

    let expected = size_root_pane(pane_1, pane_viewport(big), MIN_PANE_SIZE);
    assert_eq!(fake.resizes(pane_1).unwrap().len(), resizes_before + 1);
    assert_eq!(*fake.resizes(pane_1).unwrap().last().unwrap(), expected);
    assert_eq!(
        events,
        vec![Event::PtyResized(PtyResized {
            pane_id: pane_1,
            size: expected,
        })]
    );
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(client_c)
            .unwrap()
            .active_tab(),
        tab_2
    );
}

#[test]
fn client_attach_schedules_a_render() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    rt.bootstrap_local(
        SessionId::new(),
        Size { cols: 80, rows: 24 },
        SystemTime::now(),
    )
    .expect("bootstrap the genesis client");
    let (sid, tab_id, _pane) = only_slot(&rt);

    // Drain the render the bootstrap scheduled, so a fresh render is due only if
    // the attach schedules one.
    let now = Instant::now();
    assert!(rt.poll_render(now));
    assert!(!rt.poll_render(now));

    // A larger client attaches: it cannot shrink the tab, so no PTY reflows —
    // but the new viewer still needs a frame.
    rt.handle_client_attach(
        sid,
        ClientId::new(),
        Size {
            cols: 120,
            rows: 40,
        },
        tab_id,
        SystemTime::now(),
    );

    assert!(rt.poll_render(now + Duration::from_secs(1)));
}

#[test]
fn client_detach_schedules_a_render() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let client = rt
        .bootstrap_local(
            SessionId::new(),
            Size { cols: 80, rows: 24 },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");

    // Drain the render the bootstrap scheduled.
    let now = Instant::now();
    assert!(rt.poll_render(now));
    assert!(!rt.poll_render(now));

    // The last viewer leaves: no PTY reflows, but the detach still schedules a
    // render.
    rt.handle_client_detach(client);

    assert!(rt.poll_render(now + Duration::from_secs(1)));
}

#[test]
fn unviewed_tab_adoption_sizes_the_new_pane_to_the_pane_region() {
    let viewport = Size {
        cols: 100,
        rows: 40,
    };

    // Baseline: the same-sized client splits the tab it already views, so the
    // solve runs against the tab's drawable pane region.
    let (mut rt_viewed, fake_viewed, _tx_viewed) = new_runtime_with_fake();
    let client_viewed = ClientId::new();
    let tab_viewed = TabId::new();
    let root_viewed = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root_viewed);
    add_tab(&mut session, tab_viewed, root_viewed);
    let mut client = Client::new(
        client_viewed,
        session.id,
        SystemTime::now(),
        viewport,
        tab_viewed,
    );
    client.update_focused_pane(tab_viewed, root_viewed);
    session.attach_client(client);
    let sid_viewed = session.id;
    rt_viewed.sessions.insert(sid_viewed, session);
    rt_viewed.dispatch(envelope_from(
        CommandSource::key_binding(client_viewed),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let baseline_pane = other_pane(&rt_viewed, sid_viewed, root_viewed);
    let baseline_size = fake_viewed
        .resizes(baseline_pane)
        .expect("baseline pane spawned")[0];

    // Adoption: an identical client is designated onto an UNVIEWED tab. The
    // new pane must be fit and spawned against the client's pane region — the
    // same geometry as the viewed baseline — not the full terminal viewport.
    let (mut rt_adopt, fake_adopt, _tx_adopt) = new_runtime_with_fake();
    let client_adopt = ClientId::new();
    let tab_front = TabId::new();
    let tab_back = TabId::new();
    let pane_front = PaneId::new();
    let pane_back = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_front);
    add_pane(&mut session, pane_back);
    add_tab(&mut session, tab_front, pane_front);
    add_tab(&mut session, tab_back, pane_back);
    let mut client = Client::new(
        client_adopt,
        session.id,
        SystemTime::now(),
        viewport,
        tab_front,
    );
    client.update_focused_pane(tab_front, pane_front);
    session.attach_client(client);
    let sid_adopt = session.id;
    rt_adopt.sessions.insert(sid_adopt, session);
    rt_adopt.dispatch(envelope_from(
        CommandSource::external_cli(Some(sid_adopt)),
        Command::NewPane(NewPaneArgs {
            source: Some(pane_back),
            client: Some(client_adopt),
            ..NewPaneArgs::default()
        }),
    ));
    let adopted_pane = rt_adopt.sessions[&sid_adopt]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_front && *id != pane_back)
        .expect("the adopted split pane");
    let adopted_size = fake_adopt
        .resizes(adopted_pane)
        .expect("adopted pane spawned")[0];

    assert_eq!(adopted_size, baseline_size);
}

#[test]
fn dispatched_command_schedules_a_render() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    rt.bootstrap_local(
        SessionId::new(),
        Size { cols: 80, rows: 24 },
        SystemTime::now(),
    )
    .expect("bootstrap the genesis client");
    let (sid, _tab, pane) = only_slot(&rt);

    // Drain the render the bootstrap scheduled.
    let now = Instant::now();
    assert!(rt.poll_render(now));
    assert!(!rt.poll_render(now));

    // A command arriving outside the key path — the IPC shape — mutates the
    // layout; the dispatch itself must schedule the frame.
    let result = rt.dispatch(envelope_from(
        CommandSource::external_cli(Some(sid)),
        Command::NewPane(NewPaneArgs {
            source: Some(pane),
            ..NewPaneArgs::default()
        }),
    ));
    assert!(matches!(result, CommandResult::Ok { .. }));

    assert!(rt.poll_render(now + Duration::from_secs(1)));
}

#[test]
fn same_session_reattach_preserves_client_view_state() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let big = Size { cols: 80, rows: 24 };
    let client = rt
        .bootstrap_local(SessionId::new(), big, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (sid, tab_id, pane) = only_slot(&rt);

    // The client accumulated per-tab focus.
    rt.sessions
        .get_mut(&sid)
        .unwrap()
        .clients
        .get_mut(client)
        .unwrap()
        .update_focused_pane(tab_id, pane);

    // A re-attach of the same live id (e.g. a transport blip with no clean
    // detach) updates the view in place — it must not wipe accumulated state.
    let grown = Size {
        cols: 100,
        rows: 30,
    };
    rt.handle_client_attach(sid, client, grown, tab_id, SystemTime::now());

    let record = rt.sessions[&sid]
        .clients
        .get(client)
        .expect("still attached");
    assert_eq!(record.focused_pane(tab_id), Some(pane));
    assert_eq!(record.viewport(), grown);
    assert_eq!(record.active_tab(), tab_id);
}

#[test]
fn cross_session_attach_detaches_the_client_from_its_old_session() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let big = Size { cols: 80, rows: 24 };
    let small = Size { cols: 40, rows: 24 };

    let client = rt
        .bootstrap_local(SessionId::new(), big, SystemTime::now())
        .expect("first session");
    let (sid_1, _tab_1, pane_1) = only_slot(&rt);

    // A second, independent session with its own live pane.
    rt.bootstrap_local(SessionId::new(), big, SystemTime::now())
        .expect("second session");
    let sid_2 = *rt
        .sessions
        .keys()
        .find(|&&s| s != sid_1)
        .expect("the second session");
    let session_2 = &rt.sessions[&sid_2];
    let tab_2 = *session_2.tabs.keys().next().expect("its tab");
    let pane_2 = session_2
        .panes
        .list()
        .map(PaneRecord::id)
        .next()
        .expect("its pane");
    let pane_1_resizes_before = fake.resizes(pane_1).expect("pane spawned").len();

    // Move `client` from session 1 into session 2 at a smaller viewport.
    let events = rt.handle_client_attach(sid_2, client, small, tab_2, SystemTime::now());

    // It left session 1 entirely and is now the 40x24 co-viewer of session 2.
    assert!(rt.sessions[&sid_1].clients.get(client).is_none());
    assert_eq!(
        rt.sessions[&sid_2]
            .clients
            .get(client)
            .expect("moved into session 2")
            .active_tab(),
        tab_2
    );

    // Session 2's pane shrinks to the new minimum; session 1's pane keeps its
    // size (its tab lost its only viewer).
    let expected = size_root_pane(pane_2, pane_viewport(small), MIN_PANE_SIZE);
    assert_eq!(*fake.resizes(pane_2).unwrap().last().unwrap(), expected);
    assert_eq!(fake.resizes(pane_1).unwrap().len(), pane_1_resizes_before);
    assert_eq!(
        events,
        vec![Event::PtyResized(PtyResized {
            pane_id: pane_2,
            size: expected,
        })]
    );
}

// A split narrows its sibling: the sibling's terminal grid must re-wrap its
// content to the new width, not keep the old-width rows for the renderer to
// clip. Asserts on grid cells, not just the PtyResized event.
#[test]
fn new_pane_split_rewraps_the_sibling_grid_content() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let tab = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    add_tab(&mut session, tab, pane_a);
    let client = ClientId::new();
    add_client(&mut session, client, tab, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // First split: pane_x gets a live PTY + engine at the two-pane width.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_x = other_pane(&rt, sid, pane_a);
    let wide = rt.pty_sizes[&pane_x];
    let line: String = "A".repeat(wide.cols as usize - 2);
    let _ = rt
        .terminal_engines
        .get_mut(&pane_x)
        .unwrap()
        .advance(line.as_bytes());

    // Second split of pane_x (it holds focus): pane_x narrows.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(client),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let narrow = rt.pty_sizes[&pane_x];
    assert!(narrow.cols < wide.cols, "narrow {narrow:?} wide {wide:?}");
    let grid = rt.terminal_engines[&pane_x].state().active_grid();
    assert_eq!(grid.dimensions(), (narrow.rows, narrow.cols));
    let row0: String = grid.rows()[0]
        .iter()
        .map(koshi_terminal::grid::state::Cell::ch)
        .collect();
    let row1: String = grid.rows()[1]
        .iter()
        .map(koshi_terminal::grid::state::Cell::ch)
        .collect();
    let expect0 = "A".repeat(narrow.cols as usize);
    let rest = wide.cols as usize - 2 - narrow.cols as usize;
    let expect1 = format!(
        "{}{}",
        "A".repeat(rest),
        " ".repeat(narrow.cols as usize - rest)
    );
    assert_eq!(row0, expect0, "row0 must be a full wrapped slice");
    assert_eq!(row1, expect1, "row1 must carry the wrapped remainder");
}

// The genesis root pane goes through `bootstrap_local`, not the new-pane
// handler; its first split must still re-wrap the root's grid content.
#[test]
fn bootstrap_root_pane_rewraps_on_first_split() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let client = rt
        .bootstrap_local(
            SessionId::new(),
            Size { cols: 80, rows: 24 },
            SystemTime::now(),
        )
        .expect("bootstrap");
    let sid = *rt.sessions.keys().next().unwrap();
    let root = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .next()
        .unwrap();
    let wide = rt.pty_sizes[&root];
    let line: String = "A".repeat(wide.cols as usize - 2);
    let _ = rt
        .terminal_engines
        .get_mut(&root)
        .unwrap()
        .advance(line.as_bytes());

    rt.dispatch(envelope_from(
        CommandSource::key_binding(client),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let narrow = rt.pty_sizes[&root];
    assert!(narrow.cols < wide.cols, "narrow {narrow:?} wide {wide:?}");
    let grid = rt.terminal_engines[&root].state().active_grid();
    assert_eq!(grid.dimensions(), (narrow.rows, narrow.cols));
    let row0: String = grid.rows()[0]
        .iter()
        .map(koshi_terminal::grid::state::Cell::ch)
        .collect();
    assert_eq!(row0, "A".repeat(narrow.cols as usize));
}

#[test]
fn pane_spawn_sizes_gives_each_pane_of_a_two_pane_tab_its_own_tile() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutChild::new(LayoutNode::Pane(a)),
            LayoutChild::new(LayoutNode::Pane(b)),
        ],
    ));
    let viewport = Size { cols: 80, rows: 24 };

    // Each pane is sized to its 40-column half minus its one-cell border on
    // each side (38 content columns, 22 rows), not the whole 80-column tab.
    let sizes = pane_spawn_sizes(&tree, viewport, MIN_PANE_SIZE);
    assert_eq!(
        sizes,
        vec![
            (a, PtySize { cols: 38, rows: 22 }),
            (b, PtySize { cols: 38, rows: 22 }),
        ]
    );

    // A single pane over the same viewport keeps the full inner width, so the
    // two-pane tiles really are narrower.
    assert_eq!(
        size_root_pane(a, viewport, MIN_PANE_SIZE),
        PtySize { cols: 78, rows: 22 }
    );
}

#[test]
fn default_shell_spec_uses_the_configured_shell_and_terminal_identity() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    rt.config.terminal.default_shell = Some("/opt/homebrew/bin/fish".to_string());
    rt.config.terminal.term = "xterm-kitty".to_string();
    rt.config.terminal.colorterm = "24bit".to_string();

    let spec = rt.default_shell_spec(None, BTreeMap::new());
    assert_eq!(spec.program, PathBuf::from("/opt/homebrew/bin/fish"));
    assert_eq!(spec.shell_kind, ShellKind::Fish);
    assert_eq!(
        spec.env.get("TERM").map(String::as_str),
        Some("xterm-kitty")
    );
    assert_eq!(spec.env.get("COLORTERM").map(String::as_str), Some("24bit"));
}

#[test]
fn terminal_identity_env_keeps_a_panes_own_value_over_the_config() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    rt.config.terminal.term = "xterm-kitty".to_string();
    rt.config.terminal.colorterm = "24bit".to_string();

    // A pane that sets its own TERM keeps it; COLORTERM it left unset is filled
    // from the config.
    let mut base = BTreeMap::new();
    base.insert("TERM".to_string(), "screen-256color".to_string());
    let env = rt.terminal_identity_env(base);
    assert_eq!(env.get("TERM").map(String::as_str), Some("screen-256color"));
    assert_eq!(env.get("COLORTERM").map(String::as_str), Some("24bit"));
}

#[test]
fn effective_pane_min_floors_a_below_minimum_config_to_the_hard_minimum() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();

    // A configured minimum below the hard floor is raised to it, so a pane can
    // never be driven below the size a PTY can run at.
    rt.config.pane.min_cols = 0;
    rt.config.pane.min_rows = 0;
    assert_eq!(rt.effective_pane_min(), Size { cols: 2, rows: 1 });

    // A configured minimum above the floor is honored as written.
    rt.config.pane.min_cols = 10;
    rt.config.pane.min_rows = 5;
    assert_eq!(rt.effective_pane_min(), Size { cols: 10, rows: 5 });
}

#[test]
fn a_second_child_exit_for_the_same_pane_is_dropped_and_the_survivor_is_untouched() {
    // A `CloseOnExit` pane exits and is removed. If a duplicate exit for the now
    // gone pane arrives — two exit notices raced into the inbox — the second finds
    // no session owning the pane and drops it: no events, no panic, and the
    // surviving sibling keeps every runtime map entry it had.
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Two split panes, each with a live child and terminal engine.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    let pane_a = other_pane(&rt, sid, root);
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    // The pane that is neither the root nor the first split.
    let pane_b = rt.sessions[&sid]
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != root && *id != pane_a)
        .expect("a third pane exists");

    // First exit removes pane_a.
    let first = rt.handle_child_exit(pane_a, ExitStatus::ExitCode(0), SystemTime::UNIX_EPOCH);
    assert!(matches!(first.first(), Some(Event::PaneProcessExited(_))));
    assert!(rt.sessions[&sid].panes.get(pane_a).is_none());
    let survivor_resizes = fake.resizes(pane_b).expect("pane_b spawned").len();

    // Second exit for the same gone pane: dropped whole.
    let second = rt.handle_child_exit(pane_a, ExitStatus::ExitCode(0), SystemTime::UNIX_EPOCH);
    assert!(second.is_empty(), "a duplicate exit emits nothing");

    // The survivor is untouched: still present, still holding all its bookkeeping,
    // and not re-resized by the dropped duplicate.
    assert!(rt.sessions[&sid].panes.get(pane_b).is_some());
    assert!(rt.pty_handles.contains_key(&pane_b));
    assert!(rt.pty_sizes.contains_key(&pane_b));
    assert!(rt.terminal_engines.contains_key(&pane_b));
    assert_eq!(
        fake.resizes(pane_b).expect("pane_b spawned").len(),
        survivor_resizes,
        "the dropped duplicate reflowed nothing"
    );
}

#[test]
fn output_arriving_after_a_child_exit_is_dropped_and_a_live_pane_still_updates() {
    // Output bytes and the exit for one pane can both be waiting in the inbox. If
    // the exit is drained first the pane's engine is gone, so its trailing output
    // must be dropped without touching any state — while a still-live sibling's
    // output keeps flowing into its own engine.
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    for _ in 0..2 {
        let env = envelope_from(
            CommandSource::key_binding(client_id),
            Command::NewPane(NewPaneArgs::default()),
        );
        assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    }
    // The two spawned panes are exactly the ones with an engine; the root has none.
    let engine_panes: Vec<PaneId> = rt.terminal_engines().keys().copied().collect();
    assert_eq!(engine_panes.len(), 2, "two spawned panes hold engines");
    let (exited, live) = (engine_panes[0], engine_panes[1]);

    // The exit removes the pane and its engine.
    let _ = rt.handle_child_exit(exited, ExitStatus::ExitCode(0), SystemTime::UNIX_EPOCH);
    assert!(!rt.terminal_engines().contains_key(&exited));

    // Late output for the now-engineless pane is a no-op.
    rt.handle_pty_output(exited, b"late");
    assert!(!rt.terminal_engines().contains_key(&exited));

    // The live sibling still parses its output: two printable bytes advance its
    // cursor to column 2.
    rt.handle_pty_output(live, b"hi");
    let (row, col) = rt
        .terminal_engines()
        .get(&live)
        .expect("the live pane keeps its engine")
        .state()
        .active_cursor_position();
    assert_eq!((row, col), (0, 2));
}

#[test]
fn commands_still_dispatch_while_draining() {
    // `draining` is set the moment teardown begins, but no dispatch path consults
    // it yet — the field only records that shutdown started. Pin that documented
    // state: a valid command applied while draining still mutates and reports Ok.
    let (mut rt, _tx, client_id, sid) = lock_fixture();
    rt.draining = true;

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(lock_mode_of(&rt, sid, client_id), LockMode::Locked);
    assert!(
        rt.is_draining(),
        "dispatch does not clear the draining flag"
    );
}

#[test]
fn a_rejected_command_leaves_state_intact_and_the_next_command_works() {
    // A rejection must not be a dead end: after one command bounces off validation
    // the runtime keeps every bit of state and accepts the next command normally.
    let (mut rt, _tx, client_id, sid) = lock_fixture();

    // Close a pane that does not exist: rejected, nothing changed.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane: Some(PaneId::new()),
            force: false,
            tree: false,
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
    assert_eq!(lock_mode_of(&rt, sid, client_id), LockMode::Normal);

    // The very next command lands.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(lock_mode_of(&rt, sid, client_id), LockMode::Locked);
}

#[test]
fn a_command_after_quit_still_dispatches() {
    // `Quit` sets the loop's exit flags but does not itself gate dispatch — the
    // loop exits by polling `quit_requested`, not by dispatch refusing commands.
    // Pin that: a command issued after Quit, before the loop notices, still runs.
    let (mut rt, _tx, client_id, sid) = lock_fixture();

    assert!(matches!(
        rt.dispatch(envelope(Command::Quit)),
        CommandResult::Ok { .. }
    ));
    assert!(rt.quit_requested());

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(lock_mode_of(&rt, sid, client_id), LockMode::Locked);
}

// A rejected command leaves a warning in the log: state is untouched and the
// session carries on, which is exactly what a warning means. The line names the
// command and the reason, so the log says what the user tried and why it did
// not happen.
#[test]
fn a_rejected_command_writes_a_warning_naming_the_reason() {
    let (mut rt, _tx) = new_runtime();
    let (_guard, logs) = koshi_observability::logging::with_test_writer();

    // The client this command names is attached to no session, so validation
    // rejects it on the source before it ever resolves the tab.
    let env = envelope_from(
        CommandSource::key_binding(ClientId::new()),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(TabId::new()),
            client: None,
        }),
    );
    let command_id = env.id;
    let result = rt.dispatch(env);

    assert!(
        matches!(result, CommandResult::Rejected { .. }),
        "{result:?}"
    );
    let out = logs.contents();
    assert!(out.contains(r#""level":"WARN""#), "{out}");
    assert!(out.contains(r#""message":"command rejected""#), "{out}");
    assert!(
        out.contains(&format!(r#""command_id":"{command_id}""#)),
        "{out}"
    );
    assert!(
        out.contains(r#""reason":"source client has detached""#),
        "{out}"
    );
    // This rejection carries no hint, so the field is left off the line rather
    // than written as an empty string.
    assert!(!out.contains("help"), "{out}");
}

// A command that dispatch accepts leaves info lines, one per event it committed
// — the success side of the same trail, so the log shows what worked as well as
// what did not.
#[test]
fn an_applied_command_writes_one_info_line_per_event_it_committed() {
    let (mut rt, _tx, client_id, _sid) = lock_fixture();
    let (_guard, logs) = koshi_observability::logging::with_test_writer();

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    let out = logs.contents();
    assert_eq!(out.lines().count(), 1, "expected exactly one line: {out}");
    assert!(out.contains(r#""level":"INFO""#), "{out}");
    assert!(out.contains(r#""message":"input mode changed""#), "{out}");
    assert!(out.contains(r#""mode":"Locked""#), "{out}");
}

// --- Which client a client-scoped command lands on -------------------------
//
// A command like `koshi lock` acts on one client's own view. The client it
// means is the one that issued it, while that client is still attached. When
// the issuer is gone — or the pane was spawned with no designated client and
// names none — the session's sole attached client stands in, because with one
// window attached there is only one window the command could mean. Several
// attached, or none, has no single answer and is refused.

/// A session with one tab, one live pane, and no clients yet: the fixture the
/// acting-client rules are exercised against. Returns the runtime, the keepalive
/// sender, the session, the tab, and the pane.
fn acting_client_fixture() -> (Server, mpsc::Sender<RuntimeEvent>, SessionId, TabId, PaneId) {
    let (mut rt, tx) = new_runtime();
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane);
    add_tab(&mut session, tab, pane);
    let sid = session.id;
    rt.sessions.insert(sid, session);
    (rt, tx, sid, tab, pane)
}

#[test]
fn lock_from_a_pane_whose_client_detached_locks_the_sole_client() {
    let (mut rt, _tx, sid, tab, pane) = acting_client_fixture();
    let attached = ClientId::new();
    let detached = ClientId::new();
    let session = rt.sessions.get_mut(&sid).expect("session");
    add_client(session, attached, tab, Some(pane));

    // The pane's own client is gone, but exactly one client is attached, so
    // that one is the only window `koshi lock` could mean.
    let source = CommandSource::in_session_cli(sid, Some(detached), pane, PathBuf::from("/sock"));
    let env = envelope_from(
        source,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(lock_mode_of(&rt, sid, attached), LockMode::Locked);
}

#[test]
fn lock_from_a_clientless_pane_locks_the_sole_client() {
    let (mut rt, _tx, sid, tab, pane) = acting_client_fixture();
    let attached = ClientId::new();
    let session = rt.sessions.get_mut(&sid).expect("session");
    add_client(session, attached, tab, Some(pane));

    // A pane spawned with no designated client names none. It reads the same
    // as a client that has gone: the sole attached client stands in.
    let source = CommandSource::in_session_cli(sid, None, pane, PathBuf::from("/sock"));
    let env = envelope_from(
        source,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(lock_mode_of(&rt, sid, attached), LockMode::Locked);
}

#[test]
fn lock_from_a_detached_client_with_two_attached_is_ambiguous() {
    let (mut rt, _tx, sid, tab, pane) = acting_client_fixture();
    let first = ClientId::new();
    let second = ClientId::new();
    let detached = ClientId::new();
    let session = rt.sessions.get_mut(&sid).expect("session");
    add_client(session, first, tab, Some(pane));
    add_client(session, second, tab, Some(pane));

    // Two windows are attached and the issuer is not one of them, so there is
    // no single window to lock. Neither is guessed at.
    let source = CommandSource::in_session_cli(sid, Some(detached), pane, PathBuf::from("/sock"));
    let env = envelope_from(
        source,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some("several clients are attached; name the target client".to_string()),
        }
    );
    assert_eq!(lock_mode_of(&rt, sid, first), LockMode::Normal);
    assert_eq!(lock_mode_of(&rt, sid, second), LockMode::Normal);
}

#[test]
fn lock_from_an_attached_client_ignores_the_sole_client_fallback() {
    let (mut rt, _tx, sid, tab, pane) = acting_client_fixture();
    let other = ClientId::new();
    let issuer = ClientId::new();
    let session = rt.sessions.get_mut(&sid).expect("session");
    // The issuer attaches second, so a rule that reached for whichever client
    // came first would land on `other` and fail this test.
    add_client(session, other, tab, Some(pane));
    add_client(session, issuer, tab, Some(pane));

    // The issuer is attached, so it is the answer outright — two clients being
    // attached is only ambiguous when the issuer is not one of them.
    let source = CommandSource::in_session_cli(sid, Some(issuer), pane, PathBuf::from("/sock"));
    let env = envelope_from(
        source,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(lock_mode_of(&rt, sid, issuer), LockMode::Locked);
    assert_eq!(lock_mode_of(&rt, sid, other), LockMode::Normal);
}

#[test]
fn fullscreen_from_a_clientless_pane_zooms_the_sole_client() {
    let (mut rt, _fake, _tx, sid, client_id, _root, pane_a, _size_a) = resize_fixture();
    let tab = only_tab(&rt, sid);

    // The zoom is per-client state; with one client attached, the pane the CLI
    // was issued from fills that client's view.
    let source = CommandSource::in_session_cli(sid, None, pane_a, PathBuf::from("/sock"));
    let env = envelope_from(source, Command::TogglePaneFullscreen);
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(
        mode_of(&rt, sid, client_id, tab),
        LayoutMode::Fullscreen { focused: pane_a }
    );
}

#[test]
fn focus_tab_from_a_detached_client_falls_back_to_the_sole_client() {
    let (mut rt, _tx, sid, tab, pane) = acting_client_fixture();
    let attached = ClientId::new();
    let detached = ClientId::new();
    let second_tab = TabId::new();
    let second_pane = PaneId::new();
    let session = rt.sessions.get_mut(&sid).expect("session");
    add_client(session, attached, tab, Some(pane));
    add_pane(session, second_pane);
    add_tab(session, second_tab, second_pane);

    // A named-but-gone client falls back exactly as a source naming none does:
    // the switch lands on the one attached window.
    let source = CommandSource::in_session_cli(sid, Some(detached), pane, PathBuf::from("/sock"));
    let env = envelope_from(
        source,
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(second_tab),
            client: None,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(attached)
            .expect("client")
            .active_tab(),
        second_tab
    );
}

#[test]
fn an_explicit_client_outranks_the_sole_client_fallback() {
    let (mut rt, _tx, sid, tab, pane) = acting_client_fixture();
    let issuer = ClientId::new();
    let named = ClientId::new();
    let second_tab = TabId::new();
    let second_pane = PaneId::new();
    let session = rt.sessions.get_mut(&sid).expect("session");
    add_client(session, issuer, tab, Some(pane));
    add_client(session, named, tab, Some(pane));
    add_pane(session, second_pane);
    add_tab(session, second_tab, second_pane);

    // `--client` names the window outright: the issuing client is attached and
    // still does not win, and two attached clients are not ambiguous.
    let source = CommandSource::in_session_cli(sid, Some(issuer), pane, PathBuf::from("/sock"));
    let env = envelope_from(
        source,
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(second_tab),
            client: Some(named),
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(named)
            .expect("client")
            .active_tab(),
        second_tab
    );
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(issuer)
            .expect("client")
            .active_tab(),
        tab,
        "the issuing client's own view does not move"
    );
}

#[test]
fn an_explicit_client_that_is_not_attached_never_falls_back() {
    let (mut rt, _tx, sid, tab, pane) = acting_client_fixture();
    let attached = ClientId::new();
    let stranger = ClientId::new();
    let second_tab = TabId::new();
    let second_pane = PaneId::new();
    let session = rt.sessions.get_mut(&sid).expect("session");
    add_client(session, attached, tab, Some(pane));
    add_pane(session, second_pane);
    add_tab(session, second_tab, second_pane);

    // Naming a window that is not there is an error, not an invitation to pick
    // the one that is: a command aimed at a specific client never lands on
    // another one.
    let source = CommandSource::in_session_cli(sid, None, pane, PathBuf::from("/sock"));
    let env = envelope_from(
        source,
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(second_tab),
            client: Some(stranger),
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    assert_eq!(
        rt.sessions[&sid]
            .clients
            .get(attached)
            .expect("client")
            .active_tab(),
        tab
    );
}

#[test]
fn fullscreen_from_a_pane_on_a_tab_nobody_views_is_refused() {
    let (mut rt, _tx, sid, tab, pane) = acting_client_fixture();
    let attached = ClientId::new();
    let background_tab = TabId::new();
    let background_pane = PaneId::new();
    let session = rt.sessions.get_mut(&sid).expect("session");
    add_client(session, attached, tab, Some(pane));
    add_pane(session, background_pane);
    add_tab(session, background_tab, background_pane);

    // The fallback client is a real client, but it is looking at another tab.
    // Zooming changes what a client draws, and nobody draws this pane's tab, so
    // there is no view to change and nothing is mutated.
    let source = CommandSource::in_session_cli(sid, None, background_pane, PathBuf::from("/sock"));
    let env = envelope_from(source, Command::TogglePaneFullscreen);
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane's tab is not viewed by any client".to_string()),
        }
    );
    assert_eq!(
        mode_of(&rt, sid, attached, background_tab),
        LayoutMode::Tiled
    );
}

#[test]
fn lock_from_a_pane_on_a_background_tab_still_locks_the_sole_client() {
    let (mut rt, _tx, sid, tab, pane) = acting_client_fixture();
    let attached = ClientId::new();
    let background_tab = TabId::new();
    let background_pane = PaneId::new();
    let session = rt.sessions.get_mut(&sid).expect("session");
    add_client(session, attached, tab, Some(pane));
    add_pane(session, background_pane);
    add_tab(session, background_tab, background_pane);

    // Lock mode is the client's own state with no pane or tab in it, so which
    // tab the issuing pane sits on does not matter.
    let source = CommandSource::in_session_cli(sid, None, background_pane, PathBuf::from("/sock"));
    let env = envelope_from(
        source,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(lock_mode_of(&rt, sid, attached), LockMode::Locked);
}

// --- External targeting: acting-client defaults, tab-anchored new-pane,
// --- explicit lock client ---

#[test]
fn external_pane_default_acts_on_the_sole_clients_focused_pane() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let second = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_pane(&mut session, second);
    add_tab(&mut session, tab, root);
    let split = split_leaf(session.tabs[&tab].layout(), root, second, Direction::Right)
        .expect("root is a leaf");
    session
        .tabs
        .get_mut(&tab)
        .expect("tab")
        .update_layout(split);
    add_client(&mut session, client_id, tab, Some(second));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // No --pane: the external command acts where a keypress on the sole
    // attached client would — its focused pane, `second`.
    let source = CommandSource::external_cli(Some(sid));
    let env = envelope_from(source, Command::RenamePane(RenamePaneArgs { pane: None }));
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    // The rename landed on `second` alone: it gained a generated title and
    // `root` kept none.
    let session = &rt.sessions[&sid];
    assert!(session.panes.get(second).expect("pane").title.is_some());
    assert_eq!(session.panes.get(root).expect("pane").title, None);
}

#[test]
fn external_pane_default_with_two_clients_is_ambiguous() {
    let (mut rt, _tx) = new_runtime();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, ClientId::new(), tab, Some(root));
    add_client(&mut session, ClientId::new(), tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // Two clients could each mean a different focused pane; never guess.
    let source = CommandSource::external_cli(Some(sid));
    let env = envelope_from(source, Command::RenamePane(RenamePaneArgs { pane: None }));
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some("several clients are attached; name the target client".to_string()),
        }
    );
}

#[test]
fn external_tab_default_acts_on_the_sole_clients_active_tab() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let front_tab = TabId::new();
    let front_pane = PaneId::new();
    let back_tab = TabId::new();
    let back_pane = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, front_pane);
    add_pane(&mut session, back_pane);
    add_tab(&mut session, front_tab, front_pane);
    add_tab(&mut session, back_tab, back_pane);
    add_client(&mut session, client_id, front_tab, Some(front_pane));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // No --tab: the sole attached client's active tab is the target.
    let source = CommandSource::external_cli(Some(sid));
    let env = envelope_from(source, Command::RenameTab(RenameTabArgs { tab: None }));
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    // The rename landed on the client's active tab alone: it lost the
    // fixture name and the background tab kept it.
    let session = &rt.sessions[&sid];
    assert_ne!(session.tabs[&front_tab].name(), "t");
    assert_eq!(session.tabs[&back_tab].name(), "t");
}

#[test]
fn new_pane_with_a_tab_target_splits_that_tabs_recent_pane() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let front_tab = TabId::new();
    let front_pane = PaneId::new();
    let back_tab = TabId::new();
    let back_first = PaneId::new();
    let back_second = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, front_pane);
    add_pane(&mut session, back_first);
    add_pane(&mut session, back_second);
    add_tab(&mut session, front_tab, front_pane);
    add_tab(&mut session, back_tab, back_first);
    let split = split_leaf(
        session.tabs[&back_tab].layout(),
        back_first,
        back_second,
        Direction::Right,
    )
    .expect("back_first is a leaf");
    session
        .tabs
        .get_mut(&back_tab)
        .expect("tab")
        .update_layout(split);
    // `back_second` was focused most recently, so it is the split anchor.
    session
        .tabs
        .get_mut(&back_tab)
        .expect("tab")
        .record_focus_mru(back_second);
    add_client(&mut session, client_id, front_tab, Some(front_pane));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source: None,
            tab: Some(back_tab),
            direction: None,
            stacked: false,
            cwd: None,
            command: None,
            client: None,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    // The new pane split `back_second` (the tab's most recently focused
    // pane), so layout order reads: first, second, new.
    let leaves = rt.sessions[&sid].tabs[&back_tab].layout().leaf_panes();
    assert_eq!(leaves.len(), 3);
    assert_eq!(leaves[0], back_first);
    assert_eq!(leaves[1], back_second);
    let new_pane = leaves[2];
    // The issuing client was switched onto the target tab and focuses the
    // new pane.
    let client = rt.sessions[&sid].clients.get(client_id).expect("client");
    assert_eq!(client.active_tab(), back_tab);
    assert_eq!(client.focused_pane(back_tab), Some(new_pane));
}

#[test]
fn new_pane_tab_target_with_no_focus_history_splits_the_first_pane() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let client_id = ClientId::new();
    let front_tab = TabId::new();
    let front_pane = PaneId::new();
    let back_tab = TabId::new();
    let back_first = PaneId::new();
    let back_second = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, front_pane);
    add_pane(&mut session, back_first);
    add_pane(&mut session, back_second);
    add_tab(&mut session, front_tab, front_pane);
    add_tab(&mut session, back_tab, back_first);
    let split = split_leaf(
        session.tabs[&back_tab].layout(),
        back_first,
        back_second,
        Direction::Right,
    )
    .expect("back_first is a leaf");
    session
        .tabs
        .get_mut(&back_tab)
        .expect("tab")
        .update_layout(split);
    add_client(&mut session, client_id, front_tab, Some(front_pane));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source: None,
            tab: Some(back_tab),
            direction: None,
            stacked: false,
            cwd: None,
            command: None,
            client: None,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    // Nothing in the tab was ever focused, so the anchor falls back to the
    // first pane in layout order: the new pane splits `back_first`.
    let leaves = rt.sessions[&sid].tabs[&back_tab].layout().leaf_panes();
    assert_eq!(leaves.len(), 3);
    assert_eq!(leaves[0], back_first);
    assert_eq!(leaves[2], back_second);
}

#[test]
fn new_pane_with_an_unknown_tab_target_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source: None,
            tab: Some(TabId::new()),
            direction: None,
            stacked: false,
            cwd: None,
            command: None,
            client: None,
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
fn lock_with_an_explicit_client_locks_that_client() {
    let (mut rt, _tx) = new_runtime();
    let issuer = ClientId::new();
    let target = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, issuer, tab, Some(root));
    add_client(&mut session, target, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // --client outranks the issuer: the named client locks, the issuer does
    // not.
    let env = envelope_from(
        CommandSource::key_binding(issuer),
        Command::SetLockMode(LockModeArgs {
            locked: true,
            client: Some(target),
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(lock_mode_of(&rt, sid, target), LockMode::Locked);
    assert_eq!(lock_mode_of(&rt, sid, issuer), LockMode::Normal);
}

#[test]
fn toggle_lock_with_an_explicit_client_flips_that_client() {
    let (mut rt, _tx) = new_runtime();
    let issuer = ClientId::new();
    let target = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, issuer, tab, Some(root));
    add_client(&mut session, target, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let env = envelope_from(
        CommandSource::key_binding(issuer),
        Command::ToggleLockMode(ToggleLockModeArgs {
            client: Some(target),
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(lock_mode_of(&rt, sid, target), LockMode::Locked);
    assert_eq!(lock_mode_of(&rt, sid, issuer), LockMode::Normal);
}

#[test]
fn lock_with_an_unattached_explicit_client_is_not_found() {
    let (mut rt, _tx) = new_runtime();
    let issuer = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, issuer, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // An explicit target that is not attached refuses outright; it never
    // falls back to the issuer.
    let env = envelope_from(
        CommandSource::key_binding(issuer),
        Command::SetLockMode(LockModeArgs {
            locked: true,
            client: Some(ClientId::new()),
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    assert_eq!(lock_mode_of(&rt, sid, issuer), LockMode::Normal);
}

#[test]
fn external_lock_defaults_to_the_sole_attached_client() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab = TabId::new();
    let root = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, root);
    add_tab(&mut session, tab, root);
    add_client(&mut session, client_id, tab, Some(root));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    // `koshi lock` from outside: the sole attached client is the target.
    let source = CommandSource::external_cli(Some(sid));
    let env = envelope_from(
        source,
        Command::SetLockMode(LockModeArgs {
            locked: true,
            client: None,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));
    assert_eq!(lock_mode_of(&rt, sid, client_id), LockMode::Locked);
}
