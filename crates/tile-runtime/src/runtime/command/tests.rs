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

use tile_core::command::{
    ClosePaneArgs, CloseTabArgs, CommandSource, CopyModeCommand, EnablePluginArgs, FocusPaneArgs,
    FocusTabArgs, LockModeArgs, MoveTabArgs, NewPaneArgs, NewTabArgs, PluginCommand,
    RenamePaneArgs, RenameSessionArgs, RenameTabArgs, ResizePaneArgs, RunCommandPaneArgs,
    TabTarget, WriteToPaneArgs,
};
use tile_core::constant::GRACEFUL_TIMEOUT_DURATION;
use tile_core::geometry::{Size, SplitDirection};
use tile_core::ids::{ClientId, PaneId, PluginId, SessionId, TabId};
use tile_core::naming;
use tile_core::process::{PtySize, ShellKind, SpawnSpec};
use tile_layout::mode::LayoutMode;
use tile_layout::tree::{LayoutChild, SplitNode};
use tile_observability::cleanup::TerminalCleanupGuard;
use tile_pane::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use tile_pane::pane::state::PaneRecord;
use tile_pty::backend::state::{PtyBackend, PtyHandle};
use tile_pty::error::PtyError;
use tile_session::client::{Client, ClientRegistry};
use tile_session::session::pane_ops::NewPaneSpec;
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

/// Like [`new_runtime`], but also hands back the concrete fake backend so a test
/// can drive spawn failures and assert on spawned panes, specs, and resizes.
/// Both the runtime and the returned handle share one backend.
fn new_runtime_with_fake() -> (Runtime, Arc<FakePtyBackend>, mpsc::Sender<RuntimeEvent>) {
    let fake = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = fake.clone();
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
    (runtime, fake, tx)
}

/// The id of the single pane in `session` that is not `source` — the freshly
/// split pane. Panics unless exactly one other pane exists.
fn other_pane(rt: &Runtime, session: SessionId, source: PaneId) -> PaneId {
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
    Session::new(id, "s".to_string(), ClientRegistry::new())
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
fn client_scoped_command_without_a_client_is_unauthorized() {
    let (mut rt, _tx) = new_runtime();

    let commands = vec![
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
        force: false,
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
    let commands = vec![
        Command::CloseTab(CloseTabArgs {
            tab: Some(TabId::new()),
            force: false,
        }),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Next,
            client: None,
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

    // A new-pane anchors on a source leaf (`source: None` = the focused pane);
    // an internal source has no focused pane to anchor on. The stacked shape
    // resolves its anchor the same way, so it rejects identically.
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
            direction: Some(tile_core::geometry::Direction::Down),
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
    let source =
        CommandSource::in_session_cli(session_id, client_id, new_pane, PathBuf::from("/sock"));
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

    // The pane is already this client's focus, so the command resolves and
    // completes as a no-op: applied, zero events.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs { pane, client: None }),
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
            pane: outside,
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
        pane: PaneId::new(),
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
            pane: b,
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
            pane: b,
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
            pane: b,
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
            pane: b,
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
            pane: b,
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
            pane: r,
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
            pane,
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
            pane: b,
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
        Command::FocusPane(FocusPaneArgs { pane, client: None }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some("multiple clients; name a target client for the focus".to_string()),
        }
    );
}

#[test]
fn focus_with_no_attached_client_at_all_is_invalid() {
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
        Command::FocusPane(FocusPaneArgs { pane, client: None }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("no attached client whose focus could move".to_string()),
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
            pane: b,
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

    // Focusing collapsed `b` expands it: at an 80x24 viewport the halved
    // right column is 40x24, the stack's active member outer rect is 40x23
    // (one header row), so `b`'s PTY content becomes 38x21.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            pane: b,
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
        Some(PtySize { cols: 38, rows: 21 })
    );
    // The newly collapsed `c` keeps its last PTY size: no resize reached it.
    assert_eq!(fake.resizes(c).expect("c spawned").len(), c_resizes_before);
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
        Command::FocusPane(FocusPaneArgs { pane, client: None }),
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
        CommandSource::in_session_cli(session_id, client_id, pane_a, PathBuf::from("/sock"));
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

    // command: None resolves to the platform default shell, cwd/env inherited.
    let new_pane = other_pane(&rt, sid, root);
    assert_eq!(
        fake.spawn_spec(new_pane).unwrap(),
        SpawnSpec::default_shell(None, BTreeMap::new())
    );
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

    // An explicit command is spawned verbatim.
    let new_pane = other_pane(&rt, sid, root);
    assert_eq!(fake.spawn_spec(new_pane).unwrap(), spawn_spec());
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
        let rects = content_rects(&solve_with_mode(
            &candidate,
            LayoutMode::Tiled,
            Rect::new(Point { x: 0, y: 0 }, Size { cols: 40, rows: 10 }),
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
    }));
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert!(rt.sessions[&sid].panes.get(new_pane).is_none());
    assert_eq!(wait_for_kill(&fake, new_pane), vec![KillPolicy::Force]);
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
    Runtime,
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
            amount: 5,
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
fn resize_pane_via_in_session_cli_defaults_to_the_issuing_pane() {
    let (mut rt, _fake, _tx, sid, client_id, root, pane_a, size_a) = resize_fixture();

    // Issued from inside root's pane with no explicit target: root grows
    // right by 3, so its neighbor A donates 3 columns.
    let source = CommandSource::in_session_cli(sid, client_id, root, PathBuf::from("/sock"));
    let env = envelope_from(
        source,
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction: Direction::Right,
            amount: 3,
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
        amount: 2,
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
    let rects_before = Runtime::tab_content_rects(
        &rt.sessions[&sid],
        rt.sessions[&sid].tabs.keys().copied().next().unwrap(),
        viewport,
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
            amount: 100,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("neighbor has only 36 spare cells to give".to_string()),
        }
    );

    let rects_after = Runtime::tab_content_rects(
        &rt.sessions[&sid],
        rt.sessions[&sid].tabs.keys().copied().next().unwrap(),
        viewport,
    );
    assert_eq!(rects_after, rects_before);
    assert_eq!(fake.resizes(pane_a).unwrap().len(), resizes_before);
    assert_eq!(rt.pty_sizes[&pane_a], size_a);
}

#[test]
fn resize_pane_without_a_border_on_that_side_is_rejected() {
    let (mut rt, _fake, _tx, _sid, client_id, _root, _pane_a, _size_a) = resize_fixture();

    // A touches the tab's right edge, and the split has no vertical level at
    // all — both directions find no border to move.
    for direction in [Direction::Right, Direction::Up] {
        let env = envelope_from(
            CommandSource::key_binding(client_id),
            Command::ResizePane(ResizePaneArgs {
                pane: None,
                direction,
                amount: 1,
            }),
        );
        let command_id = env.id;
        assert_eq!(
            rt.dispatch(env),
            CommandResult::Rejected {
                command_id,
                reason: RejectReason::InvalidState,
                help: Some("pane has no neighbor on that side".to_string()),
            }
        );
    }
}

#[test]
fn resize_pane_amount_zero_is_rejected() {
    let (mut rt, _fake, _tx, _sid, client_id, _root, pane_a, size_a) = resize_fixture();

    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction: Direction::Left,
            amount: 0,
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("resize amount must be at least 1".to_string()),
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
    let rects_before = Runtime::tab_content_rects(&rt.sessions[&sid], tab, viewport);

    // No client is attached anywhere, so no tab is viewed and no terminal
    // displays the result.
    let env = envelope(Command::ResizePane(ResizePaneArgs {
        pane: Some(pane_left),
        direction: Direction::Right,
        amount: 1,
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
        Runtime::tab_content_rects(&rt.sessions[&sid], tab, viewport),
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
    let rects_before = Runtime::tab_content_rects(&rt.sessions[&sid], tab_back, viewport);

    // A client is attached, but none views the back tab — no terminal
    // displays the result, so the resize rejects and mutates nothing.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane: Some(pane_left),
            direction: Direction::Right,
            amount: 4,
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
        Runtime::tab_content_rects(&rt.sessions[&sid], tab_back, viewport),
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
            amount: 4,
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
        Command::NewTab(NewTabArgs {
            name: Some("build".to_string()),
            ..NewTabArgs::default()
        }),
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
    assert_eq!(new_tab.name(), "build");
    assert_eq!(new_tab.index(), 1);
    let new_pane = new_tab.layout().leaf_panes()[0];

    // The issuer switched onto the new tab and focuses its root pane.
    let client = session.clients.get(client_id).unwrap();
    assert_eq!(client.active_tab(), new_tab.id());
    assert_eq!(client.focused_pane(new_tab.id()), Some(new_pane));

    // The root pane is Running, records the default-shell request (no
    // explicit command), and was spawned at the client's full viewport:
    // 80x24 outer -> 78x22 content.
    let record = session.panes.get(new_pane).unwrap();
    assert_eq!(*record.lifecycle(), PaneLifecycle::Running);
    assert_eq!(record.command, None);
    assert_eq!(record.title, None);
    assert!(rt.pty_handles.contains_key(&new_pane));
    assert_eq!(
        fake.resizes(new_pane).unwrap(),
        vec![PtySize { cols: 78, rows: 22 }]
    );
    assert_eq!(rt.pty_sizes[&new_pane], PtySize { cols: 78, rows: 22 });
}

#[test]
fn new_tab_generates_a_free_name_when_none_is_given() {
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
fn new_tab_with_an_empty_name_is_rejected() {
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
            name: Some(String::new()),
            ..NewTabArgs::default()
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("tab name cannot be empty".to_string()),
        }
    );
    assert_eq!(rt.sessions[&sid].tabs.len(), 1);
    assert!(fake.spawned_panes().is_empty());
}

#[test]
fn new_tab_accepts_a_name_over_64_chars() {
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

    // The name is stored whole at any length; the renderer truncates long
    // names at display time.
    let name = "x".repeat(65);
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewTab(NewTabArgs {
            name: Some(name.clone()),
            ..NewTabArgs::default()
        }),
    );
    let command_id = env.id;
    match rt.dispatch(env) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // TabCreated, PaneCreated, TabFocused, PaneFocused, PtyResized.
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
    assert_eq!(new_tab.name(), name);
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
            help: Some("multiple clients; name a target client for the new tab".to_string()),
        }
    );
    assert!(fake.spawned_panes().is_empty());
}

#[test]
fn new_tab_with_no_attached_client_is_invalid() {
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
            reason: RejectReason::InvalidState,
            help: Some("no attached client to switch onto the new tab".to_string()),
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
    // constraint: two half-columns of 40 -> outer 20x10 -> content 18x8.
    rt.dispatch(envelope_from(
        CommandSource::key_binding(stayer),
        Command::NewPane(NewPaneArgs::default()),
    ));
    let pane_x = other_pane(&rt, sid, pane_a);
    assert_eq!(
        *fake.resizes(pane_x).unwrap().last().unwrap(),
        PtySize { cols: 18, rows: 8 }
    );

    // A creates a new tab and leaves: the vacated tab's viewport grows to
    // B's 80x24 -> outer 40x24 -> content 38x22. The new tab's root pane is
    // sized to A's own 40x10 viewport -> content 38x8.
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
        PtySize { cols: 38, rows: 22 }
    );
    let new_tab = rt.sessions[&sid]
        .tabs
        .values()
        .find(|tab| tab.id() != tab_a)
        .expect("the created tab");
    let new_pane = new_tab.layout().leaf_panes()[0];
    assert_eq!(
        fake.resizes(new_pane).unwrap(),
        vec![PtySize { cols: 38, rows: 8 }]
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
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(DummySnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(DummyStorage);
    let (_tx, inbox_rx) = mpsc::channel();
    let mut rt = Runtime::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        TerminalCleanupGuard::new(),
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

    // Split tab_a while only B views it: its PTY starts at the 80-wide
    // geometry (outer 40x24 -> content 38x22).
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
        PtySize { cols: 38, rows: 22 }
    );

    // A closes its own tab and is moved to tab_a, which now counts A's 40x10
    // viewport: outer 20x10 -> content 18x8.
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
        PtySize { cols: 18, rows: 8 }
    );
    assert_eq!(
        rt.sessions[&sid].clients.get(mover).unwrap().active_tab(),
        tab_a
    );
}

// --- RenameTab handler ---------------------------------------------------------

#[test]
fn rename_tab_updates_the_name_and_emits() {
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
        Command::RenameTab(RenameTabArgs {
            tab: None,
            name: "build".to_string(),
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
    assert_eq!(rt.sessions[&sid].tabs[&tab_a].name(), "build");
}

#[test]
fn rename_tab_to_its_current_name_is_ok_with_no_events() {
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
        Command::RenameTab(RenameTabArgs {
            tab: Some(tab_a),
            name: "t".to_string(),
        }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert!(emitted_events.is_empty()),
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(rt.sessions[&sid].tabs[&tab_a].name(), "t");
}

#[test]
fn rename_tab_with_an_empty_name_is_rejected() {
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
        Command::RenameTab(RenameTabArgs {
            tab: None,
            name: String::new(),
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("tab name cannot be empty".to_string()),
        }
    );
    assert_eq!(rt.sessions[&sid].tabs[&tab_a].name(), "t");
}

#[test]
fn rename_tab_accepts_a_name_over_64_chars() {
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

    // 65 multi-byte characters, stored whole: names have no length cap at
    // validate; the renderer truncates long names at display time.
    let name = "字".repeat(65);
    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::RenameTab(RenameTabArgs {
            tab: None,
            name: name.clone(),
        }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 1),
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(rt.sessions[&sid].tabs[&tab_a].name(), name);
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
        CommandSource::in_session_cli(session_id, client_id, pane_a, PathBuf::from("/sock"));
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
fn rename_pane_updates_the_title_and_emits() {
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

    // No explicit target: the issuer's focused pane is renamed.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::RenamePane(RenamePaneArgs {
            pane: None,
            name: "build-watch".to_string(),
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
    assert_eq!(
        rt.sessions[&sid].panes.get(pane_a).expect("pane").title,
        Some("build-watch".to_string())
    );
}

#[test]
fn rename_pane_to_its_current_title_is_ok_with_no_events() {
    let (mut rt, _tx) = new_runtime();
    let client_id = ClientId::new();
    let tab_a = TabId::new();
    let pane_a = PaneId::new();
    let mut session = bare_session(SessionId::new());
    add_pane(&mut session, pane_a);
    session.panes.get_mut(pane_a).expect("pane").title = Some("same".to_string());
    add_tab(&mut session, tab_a, pane_a);
    add_client(&mut session, client_id, tab_a, Some(pane_a));
    let sid = session.id;
    rt.sessions.insert(sid, session);

    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::RenamePane(RenamePaneArgs {
            pane: Some(pane_a),
            name: "same".to_string(),
        }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert!(emitted_events.is_empty()),
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        rt.sessions[&sid].panes.get(pane_a).expect("pane").title,
        Some("same".to_string())
    );
}

#[test]
fn rename_pane_with_an_empty_name_is_rejected() {
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
        Command::RenamePane(RenamePaneArgs {
            pane: None,
            name: String::new(),
        }),
    );
    let command_id = env.id;
    assert_eq!(
        rt.dispatch(env),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane name cannot be empty".to_string()),
        }
    );
    assert_eq!(
        rt.sessions[&sid].panes.get(pane_a).expect("pane").title,
        None
    );
}

#[test]
fn rename_pane_accepts_a_name_over_64_chars() {
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

    // 65 multi-byte characters, stored whole: names have no length cap at
    // validate; the renderer truncates long names at display time.
    let name = "字".repeat(65);
    let result = rt.dispatch(envelope_from(
        CommandSource::key_binding(client_id),
        Command::RenamePane(RenamePaneArgs {
            pane: None,
            name: name.clone(),
        }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 1),
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        rt.sessions[&sid].panes.get(pane_a).expect("pane").title,
        Some(name)
    );
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
    let env = envelope(Command::RenamePane(RenamePaneArgs {
        pane: Some(pane_a),
        name: "logs".to_string(),
    }));
    match rt.dispatch(env) {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 1),
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        rt.sessions[&sid].panes.get(pane_a).expect("pane").title,
        Some("logs".to_string())
    );
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
    let source = CommandSource::in_session_cli(sid, client_id, pane_a, PathBuf::from("/sock"));
    let result = rt.dispatch(envelope_from(
        source,
        Command::RenamePane(RenamePaneArgs {
            pane: None,
            name: "issuer".to_string(),
        }),
    ));
    match result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(emitted_events.len(), 1),
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        rt.sessions[&sid].panes.get(pane_a).expect("pane").title,
        Some("issuer".to_string())
    );
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
    let active = |rt: &Runtime| {
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
            help: Some("multiple clients; name a target client for the target tab".to_string()),
        }
    );
}

#[test]
fn focus_tab_with_no_attached_client_is_invalid() {
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
            reason: RejectReason::InvalidState,
            help: Some("no attached client to switch onto the target tab".to_string()),
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

    // Split tab_b while only B (40x10) views it: the new PTY sizes to the
    // half column — outer 20x10 -> content 18x8.
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
        PtySize { cols: 18, rows: 8 }
    );

    // A switches onto tab_b: its viewport tightens to min(40,30) x min(10,8)
    // = 30x8 — outer 15x8 -> content 13x6.
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
        PtySize { cols: 13, rows: 6 }
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
        PtySize { cols: 18, rows: 8 }
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
    Runtime,
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
fn only_tab(rt: &Runtime, sid: SessionId) -> TabId {
    rt.sessions[&sid].tabs.keys().copied().next().unwrap()
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
        session.tabs[&tab].layout_mode(),
        LayoutMode::Fullscreen { focused: pane_a }
    );
    // The mode is a solve-time overlay: the tree itself is untouched.
    assert_eq!(*session.tabs[&tab].layout(), tree_before);
    let full = PtySize { cols: 78, rows: 22 };
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

    let session = &rt.sessions[&sid];
    assert_eq!(session.tabs[&tab].layout_mode(), LayoutMode::Tiled);
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
    let source = CommandSource::in_session_cli(sid, client_id, root, PathBuf::from("/sock"));
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
        session.tabs[&tab].layout_mode(),
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
    let source = CommandSource::in_session_cli(sid, client_id, pane_b, PathBuf::from("/sock"));
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
        rt.sessions[&sid].tabs[&tab_b].layout_mode(),
        LayoutMode::Tiled
    );
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
    assert_eq!(
        rt.sessions[&sid].tabs[&tab_id].layout_mode(),
        LayoutMode::Tiled
    );
}

#[test]
fn focus_pane_under_fullscreen_retargets_the_zoom() {
    let (mut rt, _fake, _tx, sid, client_id, root, pane_a, _size_a) = fullscreen_fixture();
    let tab = only_tab(&rt, sid);
    let full = PtySize { cols: 78, rows: 22 };

    // Root is hidden behind the fullscreen — focusing it swaps the zoom to
    // it instead of rejecting or dropping the mode.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            pane: root,
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
        session.tabs[&tab].layout_mode(),
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
    let full = PtySize { cols: 78, rows: 22 };

    let focus = |pane: PaneId| {
        envelope_from(
            CommandSource::key_binding(client_id),
            Command::FocusPane(FocusPaneArgs { pane, client: None }),
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
        rt.sessions[&sid].tabs[&tab].layout_mode(),
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
            pane: pane_a,
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
        rt.sessions[&sid].tabs[&tab].layout_mode(),
        LayoutMode::Fullscreen { focused: pane_a }
    );
}

#[test]
fn focus_pane_refocusing_a_hidden_current_pane_retargets_without_a_focus_event() {
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

    // Client A zooms its own pane; client B's focus is now hidden behind it.
    let env = envelope_from(
        CommandSource::key_binding(client_a),
        Command::TogglePaneFullscreen,
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    // B re-focuses the pane it already holds: the zoom swaps to it, and no
    // focus event fires because B's focus never moved.
    let env = envelope_from(
        CommandSource::key_binding(client_b),
        Command::FocusPane(FocusPaneArgs {
            pane: b,
            client: None,
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
        session.tabs[&tab_id].layout_mode(),
        LayoutMode::Fullscreen { focused: b }
    );
    assert_eq!(
        session.clients.get(client_b).unwrap().focused_pane(tab_id),
        Some(b)
    );
}

#[test]
fn new_pane_on_a_fullscreen_tab_drops_the_fullscreen() {
    let (mut rt, _fake, _tx, sid, client_id, _root, pane_a, _size_a) = fullscreen_fixture();
    let tab = only_tab(&rt, sid);

    // Splitting the promoted pane: the tab returns to the tiled view and
    // both halves of the split are sized against it (root 40, the split
    // pair 20 each -> 18x22 content).
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::NewPane(NewPaneArgs::default()),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    let session = &rt.sessions[&sid];
    assert_eq!(session.tabs[&tab].layout_mode(), LayoutMode::Tiled);
    let pane_b = session
        .panes
        .list()
        .map(PaneRecord::id)
        .find(|id| *id != pane_a && rt.pty_sizes.contains_key(id))
        .expect("the new pane");
    let half = PtySize { cols: 18, rows: 22 };
    assert_eq!(rt.pty_sizes[&pane_a], half);
    assert_eq!(rt.pty_sizes[&pane_b], half);
}

#[test]
fn resize_pane_on_a_fullscreen_tab_drops_the_fullscreen() {
    let (mut rt, _fake, _tx, sid, client_id, _root, pane_a, size_a) = fullscreen_fixture();
    let tab = only_tab(&rt, sid);

    // The moved border must be visible: the resize lands in the tiled view.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction: Direction::Left,
            amount: 5,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    assert_eq!(
        rt.sessions[&sid].tabs[&tab].layout_mode(),
        LayoutMode::Tiled
    );
    let expected = PtySize {
        cols: size_a.cols + 5,
        rows: size_a.rows,
    };
    assert_eq!(rt.pty_sizes[&pane_a], expected);
}

#[test]
fn close_pane_hidden_behind_a_fullscreen_drops_it() {
    let (mut rt, _fake, _tx, sid, client_id, root, pane_a, _size_a) = fullscreen_fixture();
    let tab = only_tab(&rt, sid);
    let full = PtySize { cols: 78, rows: 22 };

    // Closing the hidden root: the survivor already fills the tab, so its
    // PTY keeps the full-tab size it holds.
    let env = envelope_from(
        CommandSource::key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane: Some(root),
            force: true,
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    let session = &rt.sessions[&sid];
    assert_eq!(session.tabs[&tab].layout_mode(), LayoutMode::Tiled);
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
        }),
    );
    assert!(matches!(rt.dispatch(env), CommandResult::Ok { .. }));

    let session = &rt.sessions[&sid];
    assert_eq!(session.tabs[&tab].layout_mode(), LayoutMode::Tiled);
    assert_eq!(*session.tabs[&tab].layout(), LayoutNode::Pane(root));
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab),
        Some(root)
    );
}
