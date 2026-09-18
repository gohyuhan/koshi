//! Tests for command dispatch: validation rejects ill-formed commands before
//! the match, a command that passes validation but has no handler yet routes
//! to a clean labelled rejection, and every handler the match reaches is
//! exercised — panes, tabs, clients, highlights, fullscreen, detach and the
//! session switch — together with child exits, client attach and detach, and
//! the working directory a new pane opens in.
//!
//! Rejection cases (no context) run against an empty runtime. Cases that need
//! populated state — explicit/default/focused target resolution, in-session-CLI
//! pane defaulting, and `InvalidState` session admission — build sessions with
//! the helpers below and install them into the runtime's `session_by_id` map. Cases
//! that need a live child use [`build_runtime_with_fake`], which hands back the
//! fake backend so spawns, resizes, writes and kills can be read back.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Barrier};
use std::time::{Duration, Instant, SystemTime};

use koshi_core::command::{
    ClosePaneArgs, CloseTabArgs, CommandKind, CommandSource, CopyArgs, CopyTarget,
    EnablePluginArgs, FocusPaneArgs, FocusTabArgs, GridPosition, LockModeArgs, MoveTabArgs,
    NewPaneArgs, NewTabArgs, PluginCommand, ResizePaneArgs, RunCommandPaneArgs, Selection,
    SelectionKind, TabTarget, VisualCommand, WriteToPaneArgs,
};
use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::geometry::{Direction, PixelCellSize, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, PluginId, SessionId, TabId};
use koshi_core::naming;
use koshi_core::process::{ExitStatus, PtySize, ShellKind, SpawnSpec};
use koshi_layout::edit::split_leaf;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::PaneSizing;
use koshi_layout::tree::{LayoutNode, SplitNode};
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

use crate::runtime::bus::EventFilter;
use crate::runtime::event::RuntimeEvent;
use koshi_renderer::snapshot::Delivery;

use super::*;
use koshi_core::event::QuitCause;

/// A `new-pane` request with nothing chosen: the focused pane of the issuer's
/// tab splits rightward, running the default shell.
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

/// A bare runtime with stub services and no sessions. The sender is returned so
/// the inbox stays open.
fn build_runtime() -> (Server, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let runtime = Server::from_runtime_parts(
        pty_backend,
        runtime_event_receiver,
        runtime_event_sender.clone(),
    );
    (runtime, runtime_event_sender)
}

/// Like [`build_runtime`], but also hands back the concrete fake backend so a test
/// can drive spawn failures and assert on spawned panes, specs, and resizes.
/// Both the runtime and the returned handle share one backend.
fn build_runtime_with_fake() -> (Server, Arc<FakePtyBackend>, mpsc::Sender<RuntimeEvent>) {
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let runtime = Server::from_runtime_parts(
        pty_backend,
        runtime_event_receiver,
        runtime_event_sender.clone(),
    );
    (runtime, fake_pty_backend, runtime_event_sender)
}

/// The id of the single pane in `session` that is not `source` — the freshly
/// split pane. Panics unless exactly one other pane exists.
fn find_other_pane_id(server: &Server, session_id: SessionId, source_pane_id: PaneId) -> PaneId {
    let mut other_pane_ids = server.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .filter(|pane_id| *pane_id != source_pane_id);
    let other_pane_id = other_pane_ids.next().expect("a second pane exists");
    assert!(other_pane_ids.next().is_none(), "exactly one other pane");
    other_pane_id
}

/// A minimal valid spawn request for the command-carrying variants.
fn build_spawn_spec() -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/sh"),
        arguments: Vec::new(),
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::Other("sh".to_string()),
    }
}

/// Wrap a command in an envelope from the given source with a fresh id.
fn build_command_envelope(command_source: CommandSource, command: Command) -> CommandEnvelope {
    CommandEnvelope::from_parts(CommandId::new(), command_source, SystemTime::now(), command)
}

/// Wrap a command in an internally-sourced envelope with a fresh id.
fn build_internal_command_envelope(command: Command) -> CommandEnvelope {
    build_command_envelope(CommandSource::Internal, command)
}

/// A `Starting` session with the given id and no tabs, clients, or panes.
fn build_bare_session(session_id: SessionId) -> Session {
    Session::from_identity_and_client_registry(
        session_id,
        "s".to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    )
}

/// A `Stopping` session: a tab takes it `Starting` -> `Running`, then a stop is
/// requested.
fn build_stopping_session(session_id: SessionId) -> Session {
    let mut session = build_bare_session(session_id);
    let _ = tab_ops::commit_new_tab(
        &mut session,
        TabId::new(),
        PaneId::new(),
        "t".to_string(),
        None,
        NewPaneSpec::default(),
        SystemTime::now(),
    );
    session.request_session_stop();
    session
}

/// Register a fresh `Spawning` pane in the session's registry.
fn register_pane_record(session: &mut Session, pane_id: PaneId) {
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(pane_id, SystemTime::now()))
        .expect("unique pane id");
}

/// Add a tab whose single-leaf layout is `root_pane_id`.
fn register_session_tab(session: &mut Session, tab_id: TabId, root_pane_id: PaneId) {
    let tab_index = session.tabs.len();
    session.tabs.insert(
        tab_id,
        Tab::from_root_pane(tab_id, "t".to_string(), tab_index, root_pane_id),
    );
}

/// Attach a client viewing `tab_id` that connected from `client_origin`, optionally with
/// `focused_pane_id` recorded there.
fn attach_client_with_origin(
    session: &mut Session,
    client_id: ClientId,
    tab_id: TabId,
    focused_pane_id: Option<PaneId>,
    client_origin: ClientOrigin,
) {
    let mut client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        tab_id,
        client_origin,
        "C-test-client".to_string(),
        0,
    );
    if let Some(focused_pane_id) = focused_pane_id {
        client.update_focused_pane(tab_id, focused_pane_id);
    }
    session.attach_client(client);
}

/// Attach a client with [`ClientOrigin::Local`] viewing `tab_id`, optionally with
/// `focused_pane_id` recorded there.
fn attach_client(
    session: &mut Session,
    client_id: ClientId,
    tab_id: TabId,
    focused_pane_id: Option<PaneId>,
) {
    attach_client_with_origin(
        session,
        client_id,
        tab_id,
        focused_pane_id,
        ClientOrigin::Local,
    );
}

/// Attach a client with [`ClientOrigin::Local`] viewing `tab_id` and reporting
/// `pane_area`, optionally with `focused_pane_id` recorded there.
fn attach_client_with_reported_pane_area(
    session: &mut Session,
    client_id: ClientId,
    tab_id: TabId,
    focused_pane_id: Option<PaneId>,
    pane_area: Option<PaneArea>,
) {
    let mut client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 80,
            row_count: 24,
        },
        pane_area,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    if let Some(focused_pane_id) = focused_pane_id {
        client.update_focused_pane(tab_id, focused_pane_id);
    }
    session.attach_client(client);
}

/// Poll the fake backend until `pane_id` records a kill, then return the history.
/// The close handler kills on a detached thread, so the recorded policy is
/// awaited rather than read immediately; panics if none arrives within 5s.
fn wait_for_pane_kill_policies(
    fake_pty_backend: &FakePtyBackend,
    pane_id: PaneId,
) -> Vec<KillPolicy> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let kills = fake_pty_backend
            .list_pane_kill_policies(pane_id)
            .expect("pane spawned in the fake");
        if !kills.is_empty() {
            return kills;
        }
        assert!(Instant::now() < deadline, "no kill arrived within 5s");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn passing_validation_reaches_the_unimplemented_reject() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // From an internal source (no session, no client) a store-level plugin
    // command needs no session/client/pane context, so it passes validation and
    // falls through to the not-yet-implemented arm of the match.
    let command_envelope =
        build_internal_command_envelope(Command::Plugin(PluginCommand::Enable(EnablePluginArgs {
            plugin_id: PluginId::new(),
        })));
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("plugin not yet implemented".to_string()),
        }
    );
}

#[test]
fn commands_needing_a_session_are_not_found_without_one() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // Each of these resolves a client inside the acting session, so with no
    // session there is nothing to resolve.
    let command_records = vec![
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
        Command::SetLockMode(LockModeArgs {
            is_locked: true,
            client_id: None,
        }),
        Command::TogglePaneFullscreen,
    ];

    for command in command_records {
        let command_envelope = build_internal_command_envelope(command);
        let command_id = command_envelope.command_id;
        assert_eq!(
            runtime.dispatch(command_envelope),
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
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // A highlight belongs to the client that made it, so a source naming no
    // client has no highlight to touch — never another client's.
    let command_envelope = build_internal_command_envelope(Command::Visual(
        VisualCommand::ClearSelection(ClearSelectionArgs {
            pane_id: PaneId::new(),
        }),
    ));
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: None,
        }
    );
}

#[test]
fn client_source_with_no_attached_client_is_stale() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // A keybinding names a client, but no session holds it on an empty runtime.
    let command_source = CommandSource::from_key_binding(ClientId::new());
    let command_envelope =
        build_command_envelope(command_source, Command::ClosePane(ClosePaneArgs::default()));
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: None,
        }
    );
}

/// A runtime holding one session with two tabs, one pane each, and one attached
/// client that connected from `client_origin` and views the first tab's pane. Every
/// command kind has a real target here. The sender keeps the inbox open.
fn build_command_matrix_server(
    client_origin: ClientOrigin,
) -> (Server, mpsc::Sender<RuntimeEvent>, ClientId, TabId, PaneId) {
    let (mut server, runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_tab_id = TabId::new();
    let second_pane_id = PaneId::new();

    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client_with_origin(
        &mut session,
        client_id,
        first_tab_id,
        Some(first_pane_id),
        client_origin,
    );
    server.session_by_id.insert(session.session_id, session);

    (
        server,
        runtime_event_sender,
        client_id,
        first_tab_id,
        first_pane_id,
    )
}

/// Every command kind this build has, one entry each. [`build_command_for_kind`] matches
/// over the enum, so a new variant stops the build there.
const ALL_COMMAND_KINDS: [CommandKind; 20] = [
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

/// One command of `command_kind`, aimed at `tab_id` and `pane_id` — the tab and pane the
/// acting client of [`build_command_matrix_server`] views. `SwitchSession` names a session
/// id no runtime holds, so it resolves the same way on every runtime.
fn build_command_for_kind(command_kind: CommandKind, tab_id: TabId, pane_id: PaneId) -> Command {
    match command_kind {
        CommandKind::NewPane => Command::NewPane(build_new_pane_args()),
        CommandKind::ClosePane => Command::ClosePane(ClosePaneArgs {
            pane_id: Some(pane_id),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
        CommandKind::ResizePane => Command::ResizePane(ResizePaneArgs {
            pane_id: Some(pane_id),
            direction: Direction::Left,
            resize_amount_cells: 5,
        }),
        CommandKind::FocusPane => Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(pane_id),
            client_id: None,
        }),
        CommandKind::NewTab => Command::NewTab(NewTabArgs::default()),
        CommandKind::CloseTab => Command::CloseTab(CloseTabArgs {
            tab_id: Some(tab_id),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
        CommandKind::FocusTab => Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Next,
            client_id: None,
        }),
        CommandKind::WriteToPane => Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(pane_id),
            input_bytes: vec![b'x'],
        }),
        CommandKind::ToggleLockMode => Command::ToggleLockMode(ToggleLockModeArgs::default()),
        CommandKind::SetLockMode => Command::SetLockMode(LockModeArgs {
            is_locked: true,
            client_id: None,
        }),
        CommandKind::ToggleMouseSelect => Command::ToggleMouseSelect,
        CommandKind::RunCommandPane => Command::RunCommandPane(RunCommandPaneArgs {
            spawn_spec: build_spawn_spec(),
            working_directory: None,
            source_pane_id: Some(pane_id),
            tab_id: Some(tab_id),
            direction: Direction::Right,
            should_stack: false,
            client_id: None,
        }),
        CommandKind::Visual => Command::Visual(VisualCommand::ClearSelection(ClearSelectionArgs {
            pane_id,
        })),
        CommandKind::Plugin => Command::Plugin(PluginCommand::Enable(EnablePluginArgs {
            plugin_id: PluginId::new(),
        })),
        CommandKind::TogglePaneFullscreen => Command::TogglePaneFullscreen,
        CommandKind::MoveTab => Command::MoveTab(MoveTabArgs {
            tab_id: Some(tab_id),
            target_tab_index: 1,
        }),
        CommandKind::Quit => Command::Quit,
        CommandKind::Detach => Command::Detach(DetachArgs { client_id: None }),
        CommandKind::DetachAll => Command::DetachAll,
        CommandKind::SwitchSession => Command::SwitchSession(SwitchSessionArgs {
            client_id: None,
            session_id: SessionId::new(),
        }),
    }
}

/// The variant name of each event in `event_records`, in order:
/// `[PaneCreated(..), LayoutChanged(..)]` gives
/// `["PaneCreated", "LayoutChanged"]`.
fn list_event_names(event_records: &[Event]) -> Vec<&'static str> {
    event_records.iter().map(Event::get_event_name).collect()
}

/// What a dispatch answered, with everything that differs between two runtimes
/// taken out: the names of the emitted events in order when it applied, or the
/// reason and help text when it was refused.
fn get_command_outcome(
    command_result: &CommandResult,
) -> Result<Vec<&'static str>, (RejectReason, Option<&str>)> {
    match command_result {
        CommandResult::Ok { emitted_events, .. } => Ok(list_event_names(emitted_events)),
        CommandResult::Rejected { reason, help, .. } => Err((*reason, help.as_deref())),
    }
}

/// Every session the runtime holds, in session-id order, encoded as one json
/// text. Comparing two of these compares the whole of the runtime's session
/// state — every tab, pane, client and lifecycle field — not a chosen few.
fn serialize_session_records(server: &Server) -> String {
    let mut ordered_session_records: Vec<(SessionId, &Session)> = server
        .session_by_id
        .iter()
        .map(|(session_id, session)| (*session_id, session))
        .collect();
    ordered_session_records.sort_by_key(|(session_id, _)| *session_id);
    serde_json::to_string(&ordered_session_records).expect("the sessions encode")
}

/// A runtime holding two sessions, each with one tab, one pane and one attached
/// local client. Returns the runtime, the first session's client, tab and pane,
/// and the second session's client. The sender keeps the inbox open.
fn build_two_session_server() -> (
    Server,
    mpsc::Sender<RuntimeEvent>,
    ClientId,
    TabId,
    PaneId,
    ClientId,
) {
    let (mut server, runtime_event_sender) = build_runtime();
    let first_client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut first_session = build_bare_session(SessionId::new());
    register_pane_record(&mut first_session, first_pane_id);
    register_session_tab(&mut first_session, first_tab_id, first_pane_id);
    attach_client(
        &mut first_session,
        first_client_id,
        first_tab_id,
        Some(first_pane_id),
    );
    server
        .session_by_id
        .insert(first_session.session_id, first_session);

    let second_client_id = ClientId::new();
    let second_tab_id = TabId::new();
    let second_pane_id = PaneId::new();
    let mut second_session = build_bare_session(SessionId::new());
    register_pane_record(&mut second_session, second_pane_id);
    register_session_tab(&mut second_session, second_tab_id, second_pane_id);
    attach_client(
        &mut second_session,
        second_client_id,
        second_tab_id,
        Some(second_pane_id),
    );
    server
        .session_by_id
        .insert(second_session.session_id, second_session);

    (
        server,
        runtime_event_sender,
        first_client_id,
        first_tab_id,
        first_pane_id,
        second_client_id,
    )
}

#[test]
fn a_refused_command_leaves_every_session_byte_for_byte_the_same() {
    let (mut runtime, _runtime_event_sender, _client_id, _tab_id, pane_id) =
        build_command_matrix_server(ClientOrigin::Local);
    let state_before_command = serialize_session_records(&runtime);

    // A close aimed at a real pane, from a client the session does not hold.
    // Every session encodes the same before and after, so no part of the
    // handler ran on the pane the close named.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(ClientId::new()),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(pane_id),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: None,
        }
    );
    assert_eq!(
        serialize_session_records(&runtime),
        state_before_command,
        "a refused command changed a session"
    );
}

#[test]
fn a_command_from_a_client_id_no_session_holds_is_refused_and_changes_nothing() {
    let (mut runtime, _runtime_event_sender, _client_id, _tab_id, pane_id) =
        build_command_matrix_server(ClientOrigin::Local);
    let state_before_command = serialize_session_records(&runtime);

    // The id belongs to no client anywhere, so there is no session to act in.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(ClientId::new()),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(pane_id),
            input_bytes: vec![b'x'],
        }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: None,
        }
    );
    assert_eq!(
        RejectReason::SourceClientStale.to_string(),
        "source client has detached"
    );
    assert_eq!(
        serialize_session_records(&runtime),
        state_before_command,
        "a refused command changed a session"
    );
}

#[test]
fn a_command_from_a_client_that_has_detached_is_refused_and_changes_nothing() {
    let (mut runtime, _runtime_event_sender, client_id, _tab_id, pane_id) =
        build_command_matrix_server(ClientOrigin::Local);
    let session_id = *runtime.session_by_id.keys().next().expect("the session");
    let detached_client = runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .detach_client(client_id)
        .expect("the client was attached");
    assert_eq!(detached_client.get_client_id(), client_id);
    let state_before_command = serialize_session_records(&runtime);

    // The id was attached a moment ago; the session no longer holds it.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(pane_id),
            input_bytes: vec![b'x'],
        }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: None,
        }
    );
    assert_eq!(
        RejectReason::SourceClientStale.to_string(),
        "source client has detached"
    );
    assert_eq!(
        serialize_session_records(&runtime),
        state_before_command,
        "a refused command changed a session"
    );
}

#[test]
fn a_command_naming_a_client_of_another_session_is_refused_and_changes_nothing() {
    let (mut runtime, _runtime_event_sender, first_client_id, _tab_id, _pane_id, second_client_id) =
        build_two_session_server();
    let state_before_command = serialize_session_records(&runtime);

    // The detach names a real, attached client — of the other session. The
    // acting session is the issuer's, and that client is not in it.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(first_client_id),
        Command::Detach(DetachArgs {
            client_id: Some(second_client_id),
        }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    assert_eq!(
        RejectReason::TargetNotFound.to_string(),
        "no target matched"
    );
    assert_eq!(
        serialize_session_records(&runtime),
        state_before_command,
        "a refused command changed a session"
    );
}

#[test]
fn a_client_that_connected_from_another_machine_is_admitted_like_a_local_one() {
    let (mut runtime, _runtime_event_sender, client_id, _tab_id, _pane_id) =
        build_command_matrix_server(ClientOrigin::Remote);
    let session_id = *runtime.session_by_id.keys().next().expect("the session");

    // Where the client connected from decides nothing about admission: its
    // command reaches the handler and changes its own state.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ToggleMouseSelect,
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: vec![Event::MouseSelectChanged(MouseSelectChanged {
                client_id,
                is_enabled: true,
            })],
        }
    );

    let client = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("client");
    assert!(
        client.is_mouse_selection_enabled(),
        "the toggle turned mouse-select on"
    );
    assert_eq!(client.get_origin(), ClientOrigin::Remote);
}

/// Where every client still attached to the runtime's single session connected
/// from, in client-id order.
fn list_attached_client_origins(server: &Server) -> Vec<ClientOrigin> {
    let session_id = *server.session_by_id.keys().next().expect("the session");
    server.session_by_id[&session_id]
        .clients
        .list_attached_clients()
        .map(Client::get_origin)
        .collect()
}

#[test]
fn every_command_answers_a_remote_client_the_same_as_a_local_one() {
    let mut listed_command_kinds = HashSet::new();
    let mut applied_command_kinds = Vec::new();

    for command_kind in ALL_COMMAND_KINDS {
        assert!(
            listed_command_kinds.insert(command_kind),
            "{command_kind:?} is listed twice"
        );

        // One fresh runtime per side, so a command that mutates cannot leak
        // into the next kind or across the two sides.
        let (
            mut local_runtime,
            _local_runtime_event_sender,
            local_client_id,
            local_tab_id,
            local_pane_id,
        ) = build_command_matrix_server(ClientOrigin::Local);
        let local_command = build_command_for_kind(command_kind, local_tab_id, local_pane_id);
        assert_eq!(
            local_command.get_command_kind(),
            command_kind,
            "the listed kind and the command built for it disagree"
        );
        let local_command_result = local_runtime.dispatch(build_command_envelope(
            CommandSource::from_key_binding(local_client_id),
            local_command,
        ));

        let (
            mut remote_runtime,
            _remote_runtime_event_sender,
            remote_client_id,
            remote_tab_id,
            remote_pane_id,
        ) = build_command_matrix_server(ClientOrigin::Remote);
        let remote_command_result = remote_runtime.dispatch(build_command_envelope(
            CommandSource::from_key_binding(remote_client_id),
            build_command_for_kind(command_kind, remote_tab_id, remote_pane_id),
        ));

        // The whole answer, not only whether it was refused: a refusal matches
        // reason and help text, and a success matches the emitted events one
        // for one, in order.
        let local_outcome = get_command_outcome(&local_command_result);
        assert_eq!(
            get_command_outcome(&remote_command_result),
            local_outcome,
            "{command_kind:?} answered a remote client differently from a local one"
        );

        // The same clients are left attached on both sides, and each remote one
        // still reads back as remote: dispatch never writes the origin.
        assert_eq!(
            list_attached_client_origins(&remote_runtime),
            list_attached_client_origins(&local_runtime)
                .into_iter()
                .map(|_| ClientOrigin::Remote)
                .collect::<Vec<ClientOrigin>>(),
            "{command_kind:?} left a different set of clients attached on the remote side"
        );

        if local_outcome.is_ok() {
            applied_command_kinds.push(command_kind);
        }
    }

    // The kinds that reach their handler on this fixture, so the comparison
    // above is not two matching refusals every time. The four missing ones are
    // refused by what the fixture holds, identically on both sides: a resize
    // has no border to move in a single-pane tab, a write has no running child
    // to take the bytes, a plugin command has no handler yet, and the switch
    // has no connected viewer to send the move over.
    assert_eq!(
        applied_command_kinds,
        vec![
            CommandKind::NewPane,
            CommandKind::ClosePane,
            CommandKind::FocusPane,
            CommandKind::NewTab,
            CommandKind::CloseTab,
            CommandKind::FocusTab,
            CommandKind::ToggleLockMode,
            CommandKind::SetLockMode,
            CommandKind::ToggleMouseSelect,
            CommandKind::RunCommandPane,
            CommandKind::Visual,
            CommandKind::TogglePaneFullscreen,
            CommandKind::MoveTab,
            CommandKind::Quit,
            CommandKind::Detach,
            CommandKind::DetachAll,
        ]
    );
}

#[test]
fn explicit_pane_target_absent_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    let command_envelope = build_internal_command_envelope(Command::ClosePane(ClosePaneArgs {
        pane_id: Some(PaneId::new()),
        should_force_close: false,
        should_kill_process_tree: false,
    }));
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
}

#[test]
fn default_pane_target_without_context_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // No explicit pane and an internal source: nothing to default to.
    let command_envelope =
        build_internal_command_envelope(Command::ClosePane(ClosePaneArgs::default()));
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("no session context".to_string()),
        }
    );
}

#[test]
fn write_to_pane_routes_the_pane_target() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    let command_envelope = build_internal_command_envelope(Command::WriteToPane(WriteToPaneArgs {
        pane_id: Some(PaneId::new()),
        input_bytes: vec![b'x'],
    }));
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
}

#[test]
fn write_to_a_running_pane_delivers_the_bytes() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        _client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();

    // An explicit target on a live pane injects the bytes into its child and
    // completes with no events — the write is a side effect, not a state change.
    let command_envelope = build_internal_command_envelope(Command::WriteToPane(WriteToPaneArgs {
        pane_id: Some(pane_id_a),
        input_bytes: vec![b'l', b's', b'\n'],
    }));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert!(emitted_events.is_empty());
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id_a).unwrap(),
        vec![vec![b'l', b's', b'\n']]
    );
}

/// A commanded write has no visibility guard: the bytes reach the pane's child
/// even when the layout has no room to draw the pane. `Server::find_typed_pane`
/// refuses an undrawn pane; this path does not.
#[test]
fn write_to_a_suppressed_pane_still_reaches_its_shell() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();

    // Shrink the client's terminal until the tab has no room to draw its panes.
    runtime.handle_client_resize(
        client_id,
        Size {
            column_count: 3,
            row_count: 3,
        },
        None,
    );
    assert!(
        runtime
            .build_snapshot(client_id)
            .expect("snapshot")
            .session_snapshot
            .active_tab_snapshot
            .are_all_panes_suppressed,
        "test setup: the panes must be suppressed at this size"
    );

    let command_envelope = build_internal_command_envelope(Command::WriteToPane(WriteToPaneArgs {
        pane_id: Some(pane_id_a),
        input_bytes: vec![b'l', b's'],
    }));
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id_a).unwrap(),
        vec![vec![b'l', b's']]
    );
}

/// The client's scroll offset for the pane — `0` follows live output.
fn get_client_scroll_offset(runtime: &Server, client_id: ClientId, pane_id: PaneId) -> usize {
    runtime
        .list_sessions()
        .values()
        .next()
        .unwrap()
        .clients
        .get_client_by_id(client_id)
        .unwrap()
        .get_scroll_offset(pane_id)
}

/// A client-sourced write snaps that client's scrolled-up view back to live
/// output, the same as typing the bytes into the pane. An `Internal`-sourced
/// write names no client and moves no view.
#[test]
fn a_client_sourced_write_to_pane_snaps_that_client_view_to_live_output() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();
    runtime.handle_pty_output(pane_id_a, &b"\n".repeat(200)); // push lines into history
    runtime.scroll_up(client_id, pane_id_a, 3);
    assert_eq!(get_client_scroll_offset(&runtime, client_id, pane_id_a), 3);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(pane_id_a),
            input_bytes: vec![b'l', b's', b'\n'],
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(get_client_scroll_offset(&runtime, client_id, pane_id_a), 0);
}

/// A client-sourced write drops that client's highlight in the target pane,
/// the same as typing over a selection, and leaves the view at live output.
#[test]
fn a_client_sourced_write_clears_the_clients_highlight_in_the_pane() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();
    runtime.handle_pty_output(pane_id_a, &b"\n".repeat(200));
    runtime.scroll_up(client_id, pane_id_a, 3);
    runtime
        .get_client_mut(client_id)
        .unwrap()
        .set_selection(pane_id_a, build_test_selection());

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(pane_id_a),
            input_bytes: vec![b'l', b's', b'\n'],
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(get_client_scroll_offset(&runtime, client_id, pane_id_a), 0);
    let client = runtime
        .list_sessions()
        .values()
        .next()
        .unwrap()
        .clients
        .get_client_by_id(client_id)
        .unwrap();
    assert_eq!(client.get_selection(pane_id_a), None);
}

/// An empty payload sends no bytes to the child, so it is not input: it leaves a
/// parked scrollback view exactly where it was.
#[test]
fn an_empty_client_sourced_write_leaves_a_parked_view_alone() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();
    runtime.handle_pty_output(pane_id_a, &b"\n".repeat(200));
    runtime.scroll_up(client_id, pane_id_a, 3);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(pane_id_a),
            input_bytes: Vec::new(),
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(get_client_scroll_offset(&runtime, client_id, pane_id_a), 3);
}

/// A plugin pane has no PTY, so there is nowhere for the bytes to land: the
/// write is rejected rather than aimed at a child that does not exist. The
/// pane's id still has a live PTY handle in the fake backend, so only its KIND
/// can explain the rejection.
#[test]
fn write_to_a_plugin_pane_is_rejected_and_writes_nothing() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        session_id,
        _client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();

    // Re-file `pane_a`'s pane record under `Plugin`, keeping its id and its place in
    // the layout.
    let session = runtime.session_by_id.get_mut(&session_id).expect("session");
    let created_at = session
        .panes
        .get_pane_record_by_id(pane_id_a)
        .expect("pane record")
        .get_created_at();
    session.panes.remove_pane_record(pane_id_a);
    session
        .panes
        .register_pane_record(PaneRecord::from_pane_kind(
            pane_id_a,
            PaneKind::Plugin {
                plugin_id: PluginId::new(),
            },
            created_at,
        ))
        .expect("re-inserting a removed pane id");

    let command_envelope = build_internal_command_envelope(Command::WriteToPane(WriteToPaneArgs {
        pane_id: Some(pane_id_a),
        input_bytes: vec![b'l', b's'],
    }));
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane is not a terminal pane".to_string()),
        }
    );
    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id_a).unwrap(),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn write_to_pane_defaults_to_the_clients_focused_pane() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();

    // No explicit target: a keybinding source writes to the client's focused
    // pane, which the split left on `pane_a`.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: None,
            input_bytes: vec![b'a'],
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id_a).unwrap(),
        vec![vec![b'a']]
    );
}

#[test]
fn write_to_pane_via_in_session_cli_defaults_to_the_issuing_pane() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();

    // Issued from inside `pane_a` with no explicit target: the captured issuing
    // pane is the target.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(client_id),
        pane_id_a,
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(
        command_source,
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: None,
            input_bytes: vec![b'b'],
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id_a).unwrap(),
        vec![vec![b'b']]
    );
}

#[test]
fn write_to_an_exited_pane_is_rejected() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        session_id,
        _client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();

    // Drive the live split to `Exited`; a dead pane takes no input.
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .panes
        .get_pane_record_mut_by_id(pane_id_a)
        .unwrap()
        .update_lifecycle(PaneLifecycleEvent::ProcessExited {
            exit_code: Some(0),
            exited_at: SystemTime::now(),
        })
        .unwrap();

    let command_envelope = build_internal_command_envelope(Command::WriteToPane(WriteToPaneArgs {
        pane_id: Some(pane_id_a),
        input_bytes: vec![b'x'],
    }));
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane is not accepting input".to_string()),
        }
    );
    assert!(fake_pty_backend
        .list_pane_write_bytes(pane_id_a)
        .unwrap()
        .is_empty());
}

#[test]
fn write_to_a_closing_pane_is_rejected() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        session_id,
        _client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();

    // A pane mid-teardown (`Closing`) takes no input.
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .panes
        .get_pane_record_mut_by_id(pane_id_a)
        .unwrap()
        .update_lifecycle(PaneLifecycleEvent::CloseRequested {
            close_requested_at: SystemTime::now(),
        })
        .unwrap();

    let command_envelope = build_internal_command_envelope(Command::WriteToPane(WriteToPaneArgs {
        pane_id: Some(pane_id_a),
        input_bytes: vec![b'x'],
    }));
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane is not accepting input".to_string()),
        }
    );
    assert!(fake_pty_backend
        .list_pane_write_bytes(pane_id_a)
        .unwrap()
        .is_empty());
}

#[test]
fn write_from_a_plugin_source_is_denied() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        _client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();

    // A plugin write needs the `pane_write` capability, not yet grantable, so it
    // is denied before any byte reaches the pane.
    let command_envelope = build_command_envelope(
        CommandSource::from_plugin(PluginId::new()),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(pane_id_a),
            input_bytes: vec![b'x'],
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::Unauthorized,
            help: Some("plugin lacks the pane_write capability".to_string()),
        }
    );
    assert!(fake_pty_backend
        .list_pane_write_bytes(pane_id_a)
        .unwrap()
        .is_empty());
}

#[test]
fn write_backend_failure_is_reported() {
    // A pane `Running` in the model but absent from the backend (its child died
    // between the liveness check and the write) makes the backend write fail;
    // the failure is reported, not swallowed.
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    session
        .panes
        .get_pane_record_mut_by_id(pane_id)
        .unwrap()
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .unwrap();
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_internal_command_envelope(Command::WriteToPane(WriteToPaneArgs {
        pane_id: Some(pane_id),
        input_bytes: vec![b'x'],
    }));
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane is not accepting input".to_string()),
        }
    );
}

#[test]
fn write_with_empty_data_is_a_noop_ok() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        _client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();

    // An empty payload is a legal no-op write: it applies with no events.
    let command_envelope = build_internal_command_envelope(Command::WriteToPane(WriteToPaneArgs {
        pane_id: Some(pane_id_a),
        input_bytes: Vec::new(),
    }));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
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
fn resize_pane_default_target_without_context_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    let command_envelope = build_internal_command_envelope(Command::ResizePane(ResizePaneArgs {
        pane_id: None,
        direction: koshi_core::geometry::Direction::Left,
        resize_amount_cells: 1,
    }));
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("no session context".to_string()),
        }
    );
}

#[test]
fn tab_command_without_session_context_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // An internal source has no session context to resolve a tab within.
    let command_records = vec![
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(TabId::new()),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Next,
            client_id: None,
        }),
        Command::CloseTab(CloseTabArgs::default()),
        Command::MoveTab(MoveTabArgs {
            tab_id: None,
            target_tab_index: 0,
        }),
    ];

    for command in command_records {
        let command_envelope = build_internal_command_envelope(command);
        let command_id = command_envelope.command_id;
        assert_eq!(
            runtime.dispatch(command_envelope),
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
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // These create within a session; an internal source resolves to
    // no session, so there is nothing to act on.
    let command_records = vec![
        Command::NewTab(NewTabArgs::default()),
        Command::RunCommandPane(RunCommandPaneArgs {
            spawn_spec: build_spawn_spec(),
            working_directory: None,
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Right,
            should_stack: false,
            client_id: None,
        }),
    ];

    for command in command_records {
        let command_envelope = build_internal_command_envelope(command);
        let command_id = command_envelope.command_id;
        assert_eq!(
            runtime.dispatch(command_envelope),
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
    let (mut runtime, _runtime_event_sender) = build_runtime();

    let command_envelope = build_internal_command_envelope(Command::NewPane(NewPaneArgs {
        source_pane_id: Some(PaneId::new()),
        ..build_new_pane_args()
    }));
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
}

#[test]
fn new_pane_without_an_anchor_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // A new-pane anchors on a source leaf (`source_pane_id: None` = the focused pane);
    // an internal source has no focused pane to anchor on. The stacked shape
    // resolves its anchor the same way, so it rejects identically.
    let command_cases = vec![
        build_new_pane_args(),
        NewPaneArgs {
            direction: Direction::Right,
            ..build_new_pane_args()
        },
        NewPaneArgs {
            should_stack: true,
            ..build_new_pane_args()
        },
    ];

    for command_args in command_cases {
        let command_envelope = build_internal_command_envelope(Command::NewPane(command_args));
        let command_id = command_envelope.command_id;
        assert_eq!(
            runtime.dispatch(command_envelope),
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
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // No explicit source: the focused pane anchors the split, and the new pane
    // auto-focuses, spawns its PTY, and is resized into the new geometry —
    // PaneCreated + LayoutChanged + PaneFocused + PtyResized(new pane). The root
    // has no PTY, so it contributes no PtyResized.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneCreated", "LayoutChanged", "PaneFocused", "PtyResized"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    // The split registered a second pane in the tab.
    assert_eq!(
        runtime.session_by_id[&session_id].panes.pane_record_count(),
        2
    );
}

#[test]
fn new_pane_splits_the_direction_the_command_names() {
    // The command carries the direction outright and the handler obeys it. The
    // direction here is Down, so a handler that split rightward — its own
    // default, or the stock setting — fails.
    let (mut runtime, _runtime_event_sender) = build_runtime();

    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let layout_before_split = runtime.session_by_id[&session_id].tabs[&tab_id]
        .get_layout_tree()
        .clone();
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            direction: Direction::Down,
            ..build_new_pane_args()
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { .. } => {}
        other => panic!("expected Ok, got {other:?}"),
    }

    let new_pane_id = find_other_pane_id(&runtime, session_id, pane_id);
    let expected_layout = split_leaf(&layout_before_split, pane_id, new_pane_id, Direction::Down)
        .expect("split on the source leaf");
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &expected_layout
    );
}

#[test]
fn new_pane_stacked_on_a_plain_leaf_creates_a_stack() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // A stacked new-pane on a plain leaf turns the leaf into a two-member
    // stack: the source collapses to a header, the new pane is the expanded
    // active member and takes focus — PaneCreated + LayoutChanged +
    // PaneFocused + PtyResized(new pane).
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            should_stack: true,
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneCreated", "LayoutChanged", "PaneFocused", "PtyResized"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let new_pane_id = find_other_pane_id(&runtime, session_id, pane_id);
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![pane_id, new_pane_id],
            1
        ))
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(new_pane_id)
    );
    assert!(runtime.pty_handle_by_pane_id.contains_key(&new_pane_id));
}

#[test]
fn new_pane_stacked_onto_a_stack_member_appends() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (first_stack_pane_id, second_stack_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_stack_pane_id);
    register_pane_record(&mut session, second_stack_pane_id);
    register_session_tab(&mut session, tab_id, first_stack_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .unwrap()
        .update_layout(LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![first_stack_pane_id, second_stack_pane_id],
            1,
        )));
    attach_client(&mut session, client_id, tab_id, Some(second_stack_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Stacking from a pane already inside a stack appends to that stack: the
    // new pane joins as the last member, becomes the expanded active one, and
    // every earlier member collapses.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            should_stack: true,
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneCreated", "LayoutChanged", "PaneFocused", "PtyResized"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let new_pane_id = {
        let mut others = runtime.session_by_id[&session_id]
            .panes
            .list_pane_records()
            .map(PaneRecord::get_pane_id)
            .filter(|pane_id| *pane_id != first_stack_pane_id && *pane_id != second_stack_pane_id);
        let new_pane_id = others.next().expect("a third pane exists");
        assert!(others.next().is_none(), "exactly one new pane");
        new_pane_id
    };
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![first_stack_pane_id, second_stack_pane_id, new_pane_id],
            2,
        ))
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(new_pane_id)
    );
}

#[test]
fn new_pane_stacked_ignores_direction() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // `--stacked` with a direction still stacks — a stack has no direction, so
    // the flag routes to the stack edit and the direction is never read.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            should_stack: true,
            direction: Direction::Down,
            ..build_new_pane_args()
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { .. } => {}
        other => panic!("expected Ok, got {other:?}"),
    }

    let new_pane_id = find_other_pane_id(&runtime, session_id, pane_id);
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![pane_id, new_pane_id],
            1
        ))
    );
}

#[test]
fn new_pane_stacked_with_a_missing_source_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    runtime.session_by_id.insert(session.session_id, session);

    // A stacked request naming a pane that does not exist resolves its target
    // like any other new-pane and rejects `TargetNotFound`.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            should_stack: true,
            source_pane_id: Some(PaneId::new()),
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
}

#[test]
fn new_pane_stacked_with_no_space_is_min_size() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    // A 2x1 viewport cannot hold a stack: a two-member stack needs one header
    // row plus the active member's minimum rows.
    let mut client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 2,
            row_count: 1,
        },
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, pane_id);
    session.attach_client(client);
    runtime.session_by_id.insert(session.session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            should_stack: true,
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("not enough space for a new pane".to_string()),
        }
    );
}

#[test]
fn new_pane_stacked_spawn_failure_leaves_no_trace() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    fake_pty_backend.fail_spawns_with(PtyError::Spawn {
        detail: "boom".to_string(),
    });
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    let before_layout = runtime.session_by_id[&session_id].tabs[&tab_id]
        .get_layout_tree()
        .clone();

    // Launch-then-commit holds for the stacked shape too: the child cannot
    // launch, so no stack is created and nothing is committed.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            should_stack: true,
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("failed to launch the pane's process".to_string()),
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id].panes.pane_record_count(),
        1
    );
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &before_layout
    );
    assert!(runtime.pty_handle_by_pane_id.is_empty());
    assert!(fake_pty_backend.list_spawned_pane_ids().is_empty());
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(root_pane_id)
    );
}

#[test]
fn new_pane_with_no_space_is_min_size() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    // A 2x1 viewport cannot hold a split at minimum size.
    let mut client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 2,
            row_count: 1,
        },
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, pane_id);
    session.attach_client(client);
    runtime.session_by_id.insert(session.session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("not enough space for a new pane".to_string()),
        }
    );
}

#[test]
fn new_pane_explicit_pane_in_session_without_clients_is_rejected() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // Session A holds the acting client.
    let client_id = ClientId::new();
    let session_id_a = SessionId::new();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session_a = build_bare_session(session_id_a);
    register_pane_record(&mut session_a, pane_id_a);
    register_session_tab(&mut session_a, tab_id_a, pane_id_a);
    attach_client(&mut session_a, client_id, tab_id_a, Some(pane_id_a));
    runtime.session_by_id.insert(session_id_a, session_a);

    // Session B owns the explicit `--pane` target and has no client at all, so
    // nothing can view the new pane's tab.
    let session_id_b = SessionId::new();
    let tab_id_b = TabId::new();
    let pane_id_b = PaneId::new();
    let mut session_b = build_bare_session(session_id_b);
    register_pane_record(&mut session_b, pane_id_b);
    register_session_tab(&mut session_b, tab_id_b, pane_id_b);
    runtime.session_by_id.insert(session_id_b, session_b);

    // A global `--pane` targets B, but B has no client to adopt onto the tab, so
    // the pane could never be sized or shown: reject rather than strand it.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(pane_id_b),
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("no attached client to view the new pane's tab".to_string()),
        }
    );
    // Rejected before mutating: neither session grew a pane.
    assert_eq!(
        runtime.session_by_id[&session_id_b]
            .panes
            .pane_record_count(),
        1
    );
    assert_eq!(
        runtime.session_by_id[&session_id_a]
            .panes
            .pane_record_count(),
        1
    );
}

#[test]
fn new_pane_with_stale_focus_outside_active_tab_is_rejected() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let leaf_pane_id = PaneId::new();
    let unregistered_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, leaf_pane_id);
    // `unregistered_pane_id` is registered but never placed in the tab's layout.
    register_pane_record(&mut session, unregistered_pane_id);
    register_session_tab(&mut session, tab_id, leaf_pane_id);
    // The client's active-tab focus points at `unregistered_pane_id` — a stale entry that is
    // not a leaf of the active tab.
    attach_client(&mut session, client_id, tab_id, Some(unregistered_pane_id));
    runtime.session_by_id.insert(session.session_id, session);

    // The default source is the stale focus; it must reject, never split a tab.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("pane not in the client's active tab".to_string()),
        }
    );
}

#[test]
fn rejection_keys_back_to_the_originating_command_id() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // A store-level plugin command passes validation from an internal source,
    // so it reaches the match and the reject keys back to its command id.
    let command_envelope =
        build_internal_command_envelope(Command::Plugin(PluginCommand::Enable(EnablePluginArgs {
            plugin_id: PluginId::new(),
        })));
    let command_id = command_envelope.command_id;

    match runtime.dispatch(command_envelope) {
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
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The pane exists in the registry but no tab's layout holds it: validation
    // (registry membership) passes, and the handler's own tab lookup rejects
    // before anything mutates.
    let command_envelope = build_internal_command_envelope(Command::ClosePane(ClosePaneArgs {
        pane_id: Some(pane_id),
        should_force_close: false,
        should_kill_process_tree: false,
    }));
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id].panes.pane_record_count(),
        1
    );
}

#[test]
fn explicit_pane_in_stopping_session_is_invalid_state() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let pane_id = PaneId::new();
    let mut session = build_stopping_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    runtime.session_by_id.insert(session.session_id, session);

    // An internal source has no acting session, so admission is reached only
    // via the pane's owning session — which is stopping.
    let command_envelope = build_internal_command_envelope(Command::ClosePane(ClosePaneArgs {
        pane_id: Some(pane_id),
        should_force_close: false,
        should_kill_process_tree: false,
    }));
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("session is stopping".to_string()),
        }
    );
}

#[test]
fn in_session_cli_close_defaults_to_its_source_pane() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(session_id);
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    runtime.session_by_id.insert(session_id, session);

    // Grow a second pane; the split focuses it and parks its handle.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);

    // No explicit pane: an in-session CLI closes the pane it was issued from.
    // The captured pane is the split one, so the root survives and inherits
    // focus — PaneClosing + PaneRemoved + LayoutChanged + PaneFocused.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(client_id),
        new_pane_id,
        PathBuf::from("/sock"),
    );
    let command_envelope =
        build_command_envelope(command_source, Command::ClosePane(ClosePaneArgs::default()));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .is_none());
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(root_pane_id)
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(root_pane_id)
    );
    assert!(!runtime.pty_handle_by_pane_id.contains_key(&new_pane_id));
}

#[test]
fn in_session_cli_with_missing_source_pane_is_gone() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let mut session = build_bare_session(session_id);
    attach_client(&mut session, client_id, TabId::new(), None);
    runtime.session_by_id.insert(session.session_id, session);

    // The source pane has since closed; the command issued from it is refused
    // before any target resolution.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(client_id),
        PaneId::new(),
        PathBuf::from("/sock"),
    );
    let command_envelope =
        build_command_envelope(command_source, Command::ClosePane(ClosePaneArgs::default()));
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
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
    let (mut runtime, runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    (runtime, runtime_event_sender, client_id, session_id)
}

/// Read `client_id`'s lock mode out of session `session_id`.
fn lock_mode_of(runtime: &Server, session_id: SessionId, client_id: ClientId) -> LockMode {
    runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_lock_mode()
}

#[test]
fn toggle_lock_mode_locks_an_unlocked_client() {
    let (mut runtime, _runtime_event_sender, client_id, session_id) = lock_fixture();

    // A default-Normal client toggles into Locked: exactly one event.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(list_event_names(&emitted_events), ["InputModeChanged"]);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        lock_mode_of(&runtime, session_id, client_id),
        LockMode::Locked
    );
}

#[test]
fn toggle_mouse_select_flips_the_client_flag() {
    let (mut runtime, _runtime_event_sender, client_id, session_id) = lock_fixture();
    let is_mouse_selection_enabled = |runtime: &Server| {
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .is_mouse_selection_enabled()
    };
    assert!(
        !is_mouse_selection_enabled(&runtime),
        "a fresh client does not grab the mouse"
    );

    // First toggle turns mouse-select on and reports the new value, which is
    // what the viewer routes its own mouse events against.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ToggleMouseSelect,
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: vec![Event::MouseSelectChanged(MouseSelectChanged {
                client_id,
                is_enabled: true,
            })],
        }
    );
    assert!(
        is_mouse_selection_enabled(&runtime),
        "the toggle turned mouse-select on"
    );

    // A second toggle turns it back off, and reports that too.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ToggleMouseSelect,
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: vec![Event::MouseSelectChanged(MouseSelectChanged {
                client_id,
                is_enabled: false,
            })],
        }
    );
    assert!(
        !is_mouse_selection_enabled(&runtime),
        "the second toggle turned mouse-select off"
    );
}

#[test]
fn toggle_lock_mode_unlocks_a_locked_client() {
    let (mut runtime, _runtime_event_sender, client_id, session_id) = lock_fixture();
    runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("session")
        .clients
        .get_client_mut_by_id(client_id)
        .expect("client")
        .update_lock_mode(LockMode::Locked);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(list_event_names(&emitted_events), ["InputModeChanged"]);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        lock_mode_of(&runtime, session_id, client_id),
        LockMode::Normal
    );
}

#[test]
fn set_lock_mode_locks_then_unlocks() {
    let (mut runtime, _runtime_event_sender, client_id, session_id) = lock_fixture();

    let lock = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::SetLockMode(LockModeArgs {
            is_locked: true,
            client_id: None,
        }),
    );
    let lock_id = lock.command_id;
    match runtime.dispatch(lock) {
        CommandResult::Ok {
            command_id,
            emitted_events,
        } => {
            assert_eq!(command_id, lock_id);
            assert_eq!(list_event_names(&emitted_events), ["InputModeChanged"]);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        lock_mode_of(&runtime, session_id, client_id),
        LockMode::Locked
    );

    let unlock = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::SetLockMode(LockModeArgs {
            is_locked: false,
            client_id: None,
        }),
    );
    let unlock_id = unlock.command_id;
    match runtime.dispatch(unlock) {
        CommandResult::Ok {
            command_id,
            emitted_events,
        } => {
            assert_eq!(command_id, unlock_id);
            assert_eq!(list_event_names(&emitted_events), ["InputModeChanged"]);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        lock_mode_of(&runtime, session_id, client_id),
        LockMode::Normal
    );
}

#[test]
fn setting_the_current_lock_mode_emits_nothing() {
    let (mut runtime, _runtime_event_sender, client_id, session_id) = lock_fixture();

    // The client is already Normal; unlocking it changes nothing.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::SetLockMode(LockModeArgs {
            is_locked: false,
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events, Vec::new());
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        lock_mode_of(&runtime, session_id, client_id),
        LockMode::Normal
    );
}

#[test]
fn lock_mode_is_isolated_between_clients() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let (alice, bob) = (ClientId::new(), ClientId::new());
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    // Both clients view the same tab and pane, proving lock is per-client, not
    // per-pane.
    attach_client(&mut session, alice, tab_id, Some(pane_id));
    attach_client(&mut session, bob, tab_id, Some(pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(alice),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let _ = runtime.dispatch(command_envelope);

    assert_eq!(lock_mode_of(&runtime, session_id, alice), LockMode::Locked);
    assert_eq!(lock_mode_of(&runtime, session_id, bob), LockMode::Normal);
}

#[test]
fn lock_mode_toggles_without_a_focused_pane() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let mut session = build_bare_session(SessionId::new());
    // No pane, no focus: lock is client-scoped, so it still applies.
    attach_client(&mut session, client_id, TabId::new(), None);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(list_event_names(&emitted_events), ["InputModeChanged"]);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        lock_mode_of(&runtime, session_id, client_id),
        LockMode::Locked
    );
}

#[test]
fn client_without_focused_pane_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let mut session = build_bare_session(SessionId::new());
    attach_client(&mut session, client_id, TabId::new(), None);
    runtime.session_by_id.insert(session.session_id, session);

    // Fullscreen acts on the focused pane; this client has none.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::TogglePaneFullscreen,
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("no focused pane".to_string()),
        }
    );
}

#[test]
fn focused_pane_that_no_longer_exists_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    // Focus records a pane that was never registered (or has since been removed):
    // resolution checks the registry, not just that a focus is recorded.
    let mut session = build_bare_session(SessionId::new());
    attach_client(&mut session, client_id, TabId::new(), Some(PaneId::new()));
    runtime.session_by_id.insert(session.session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::TogglePaneFullscreen,
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
}

#[test]
fn a_highlight_command_names_its_own_pane_and_ignores_the_focused_one() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let focused_pane_id = PaneId::new();
    let other_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, focused_pane_id);
    register_pane_record(&mut session, other_pane_id);
    register_session_tab(&mut session, tab_id, focused_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(focused_pane_id));
    runtime.session_by_id.insert(session.session_id, session);

    // Highlight the pane that is NOT focused: the command names it, so the
    // focused pane is not consulted and never falls in as a default.
    let selection = build_test_selection();
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::Visual(VisualCommand::SetSelection(SetSelectionArgs {
            pane_id: other_pane_id,
            selection,
        })),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    let client = runtime.get_client_mut(client_id).expect("client");
    assert_eq!(
        client.get_selection(other_pane_id),
        Some(selection),
        "the named pane is highlighted"
    );
    assert_eq!(
        client.get_selection(focused_pane_id),
        None,
        "the focused pane is untouched"
    );
}

#[test]
fn a_highlight_command_for_a_pane_that_is_gone_is_rejected() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    runtime.session_by_id.insert(session.session_id, session);

    // A drag that raced the pane closing names a pane the session no longer has.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::Visual(VisualCommand::SetSelection(SetSelectionArgs {
            pane_id: PaneId::new(),
            selection: build_test_selection(),
        })),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetGone,
            help: None,
        }
    );
}

#[test]
fn clearing_a_pane_with_no_highlight_is_accepted_and_changes_nothing() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    runtime.session_by_id.insert(session.session_id, session);

    // The ways a highlight ends fire without first checking one was up, so
    // clearing nothing must not be an error.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::Visual(VisualCommand::ClearSelection(ClearSelectionArgs {
            pane_id,
        })),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        runtime
            .get_client_mut(client_id)
            .expect("client")
            .get_selection(pane_id),
        None
    );
}

#[test]
fn clearing_a_highlight_in_a_pane_the_session_lost_is_target_gone() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    runtime.session_by_id.insert(session.session_id, session);

    // Setting and clearing a highlight answer one pane the same way: a pane the
    // session does not hold is gone for both.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::Visual(VisualCommand::ClearSelection(ClearSelectionArgs {
            pane_id: PaneId::new(),
        })),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetGone,
            help: None,
        }
    );
}

#[test]
fn a_host_paste_lands_whole_in_the_focused_pane() {
    // The OS paste key pressed in the outer terminal: the text arrives as one
    // block and is written whole — a pasted Tab lands in the shell instead of
    // firing the tab-switch binding.
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _pty_size,
    ) = build_resize_fixture();

    runtime.handle_host_paste(client_id, "ls\ttmp\ncat");
    let input_write_batches = fake_pty_backend
        .list_pane_write_bytes(pane_id_a)
        .expect("pane writes");
    assert_eq!(
        input_write_batches.last().expect("one write"),
        b"ls\ttmp\rcat",
        "raw bytes, line break as the Enter byte"
    );
}

#[test]
fn a_host_paste_wraps_in_bracketed_markers_when_the_pane_turned_them_on() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _pty_size,
    ) = build_resize_fixture();
    runtime.handle_pty_output(pane_id_a, b"\x1b[?2004h");

    runtime.handle_host_paste(client_id, "ok");
    let input_write_batches = fake_pty_backend
        .list_pane_write_bytes(pane_id_a)
        .expect("pane writes");
    assert_eq!(
        input_write_batches.last().expect("one write"),
        b"\x1b[200~ok\x1b[201~"
    );
}

#[test]
fn a_host_paste_clears_the_highlight_in_the_pasted_pane() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _pty_size,
    ) = build_resize_fixture();
    let client = runtime.get_client_mut(client_id).expect("client");
    client.set_selection(pane_id_a, build_test_selection());

    runtime.handle_host_paste(client_id, "x");
    assert_eq!(
        runtime
            .get_client_mut(client_id)
            .expect("client")
            .get_selection(pane_id_a),
        None,
        "pasted text reached the child, so the highlight is gone"
    );
}

#[test]
fn copying_a_pane_with_no_highlight_writes_nothing_and_is_not_an_error() {
    // A plain click ends a gesture that highlighted nothing, so the copy it
    // dispatches finds nothing to copy. That is a no-op, not a rejection.
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _pty_size,
    ) = build_resize_fixture();

    let command_envelope = build_command_envelope(
        CommandSource::from_mouse(client_id),
        Command::Visual(VisualCommand::Copy(CopyArgs {
            pane_id: pane_id_a,
            should_trim_trailing_whitespace: true,
            clipboard_target: CopyTarget::Osc52,
        })),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
    assert_eq!(runtime.take_host_writes(client_id), None);
}

/// A one-cell character highlight, for tests that care which pane a command
/// lands on rather than what it highlights.
fn build_test_selection() -> Selection {
    Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 0,
            column_index: 1,
        },
    }
}

#[test]
fn focus_pane_in_the_active_tab_resolves() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    runtime.session_by_id.insert(session.session_id, session);

    // The pane is already this client's focus, so the command resolves and
    // completes as a no-op: applied, zero events.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(pane_id),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(emitted_events, Vec::new());
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn focus_pane_by_direction_with_no_neighbor_is_target_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    runtime.session_by_id.insert(session.session_id, session);

    // A fully valid session context whose sole pane has no left neighbor:
    // the geometric lookup itself reports the miss.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Direction(Direction::Left),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("no pane in that direction".to_string()),
        }
    );
}

#[test]
fn quit_from_a_source_with_no_client_marks_immediate_teardown() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // An internal source names no client, so quit ends the process instead of
    // detaching one.
    let command_envelope = build_internal_command_envelope(Command::Quit);
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
    assert!(runtime.is_quit_requested());
    assert!(runtime.should_shutdown_immediately);
}

#[test]
fn focus_pane_outside_the_active_tab_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let outside_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    // `outside_pane_id` exists in the registry but is not in the active tab's layout,
    // proving the check is tab-scoped, not mere global existence.
    register_pane_record(&mut session, outside_pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    runtime.session_by_id.insert(session.session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(outside_pane_id),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("pane not in the client's active tab".to_string()),
        }
    );
}

#[test]
fn focus_from_a_sessionless_source_has_no_session_context() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // An internal source names neither client nor session; the resolver has
    // no session to find a target client in.
    let command_envelope = build_internal_command_envelope(Command::FocusPane(FocusPaneArgs {
        focus_target: FocusTarget::Pane(PaneId::new()),
        client_id: None,
    }));
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("no session context".to_string()),
        }
    );
}

#[test]
fn focus_pane_moves_focus_records_mru_and_emits_one_event() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (focused_pane_id, target_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, focused_pane_id);
    register_pane_record(&mut session, target_pane_id);
    register_session_tab(&mut session, tab_id, focused_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(focused_pane_id),
                LayoutNode::Pane(target_pane_id),
            ],
        )));
    attach_client(&mut session, client_id, tab_id, Some(focused_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(target_pane_id),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // Exactly the focus fact: a plain move changes no layout and no PTY.
            assert_eq!(list_event_names(&emitted_events), ["PaneFocused"]);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane(tab_id),
        Some(target_pane_id)
    );
    assert_eq!(
        session.tabs[&tab_id].list_focus_mru().first(),
        Some(&target_pane_id)
    );
}

#[test]
fn focus_suppressed_pane_is_rejected_and_mutates_nothing() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (focused_pane_id, suppressed_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, focused_pane_id);
    register_pane_record(&mut session, suppressed_pane_id);
    register_session_tab(&mut session, tab_id, focused_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Vertical,
            vec![
                LayoutNode::Pane(focused_pane_id),
                LayoutNode::Pane(suppressed_pane_id),
            ],
        )));
    // A 2x1 viewport is below every pane's border-inclusive floor, so the
    // solve suppresses the whole split — the second pane cannot take focus.
    let mut client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 2,
            row_count: 1,
        },
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, focused_pane_id);
    session.attach_client(client);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(suppressed_pane_id),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane is suppressed; not enough space to show it".to_string()),
        }
    );
    // Nothing moved: focus and MRU are untouched.
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane(tab_id),
        Some(focused_pane_id)
    );
    assert!(!session.tabs[&tab_id]
        .list_focus_mru()
        .contains(&suppressed_pane_id));
}

#[test]
fn focus_collapsed_stack_member_activates_the_stack() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (active_pane_id, collapsed_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, active_pane_id);
    register_pane_record(&mut session, collapsed_pane_id);
    register_session_tab(&mut session, tab_id, active_pane_id);
    // The active pane is expanded; the second pane is collapsed to a header strip.
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![active_pane_id, collapsed_pane_id],
            0,
        )));
    attach_client(&mut session, client_id, tab_id, Some(active_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(collapsed_pane_id),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // LayoutChanged (the stack swapped members) + PaneFocused. No
            // PtyResized: neither pane has a live PTY here.
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PaneFocused"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![active_pane_id, collapsed_pane_id],
            1,
        ))
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane(tab_id),
        Some(collapsed_pane_id)
    );
    assert_eq!(
        session.tabs[&tab_id].list_focus_mru().first(),
        Some(&collapsed_pane_id)
    );
}

#[test]
fn focus_active_stack_member_changes_no_layout() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (left_pane_id, active_stack_pane_id, collapsed_stack_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, left_pane_id);
    register_pane_record(&mut session, active_stack_pane_id);
    register_pane_record(&mut session, collapsed_stack_pane_id);
    register_session_tab(&mut session, tab_id, left_pane_id);
    // The active stack pane is expanded: focusing it needs no activation.
    let layout = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(left_pane_id),
            LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                vec![active_stack_pane_id, collapsed_stack_pane_id],
                0,
            )),
        ],
    ));
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(layout.clone());
    attach_client(&mut session, client_id, tab_id, Some(left_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(active_stack_pane_id),
            client_id: None,
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => {
            // Only the focus fact — the tree is untouched.
            assert_eq!(list_event_names(&emitted_events), ["PaneFocused"]);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs[&tab_id].get_layout_tree(), &layout);
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane(tab_id),
        Some(active_stack_pane_id)
    );
}

#[test]
fn focus_already_focused_collapsed_member_reactivates_without_a_focus_event() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (active_pane_id, collapsed_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, active_pane_id);
    register_pane_record(&mut session, collapsed_pane_id);
    register_session_tab(&mut session, tab_id, active_pane_id);
    // The client's focus already points at the collapsed pane, but it sits collapsed (as
    // happens when another actor swaps the stack's active member).
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![active_pane_id, collapsed_pane_id],
            0,
        )));
    attach_client(&mut session, client_id, tab_id, Some(collapsed_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(collapsed_pane_id),
            client_id: None,
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => {
            // LayoutChanged only: the stack expands `b`, but the focus did not
            // move, so no PaneFocused is emitted.
            assert_eq!(list_event_names(&emitted_events), ["LayoutChanged"]);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![active_pane_id, collapsed_pane_id],
            1,
        ))
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane(tab_id),
        Some(collapsed_pane_id)
    );
}

#[test]
fn focus_explicit_client_wins_over_the_issuer() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let issuer_client_id = ClientId::new();
    let target_client_id = ClientId::new();
    let issuer_tab_id = TabId::new();
    let target_tab_id = TabId::new();
    let (issuer_pane_id, target_focused_pane_id, target_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, issuer_pane_id);
    register_pane_record(&mut session, target_focused_pane_id);
    register_pane_record(&mut session, target_pane_id);
    register_session_tab(&mut session, issuer_tab_id, issuer_pane_id);
    register_session_tab(&mut session, target_tab_id, target_focused_pane_id);
    session
        .tabs
        .get_mut(&target_tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(target_focused_pane_id),
                LayoutNode::Pane(target_pane_id),
            ],
        )));
    attach_client(
        &mut session,
        issuer_client_id,
        issuer_tab_id,
        Some(issuer_pane_id),
    );
    attach_client(
        &mut session,
        target_client_id,
        target_tab_id,
        Some(target_focused_pane_id),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The target pane is not in the issuer's active tab — it resolves against the
    // NAMED client's active tab, proving the explicit target wins.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(issuer_client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(target_pane_id),
            client_id: Some(target_client_id),
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(list_event_names(&emitted_events), ["PaneFocused"]);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(target_client_id)
            .expect("target client")
            .get_focused_pane(target_tab_id),
        Some(target_pane_id)
    );
    // The issuer's own focus is untouched.
    assert_eq!(
        session
            .clients
            .get_client_by_id(issuer_client_id)
            .expect("issuer client")
            .get_focused_pane(issuer_tab_id),
        Some(issuer_pane_id)
    );
}

#[test]
fn focus_unattached_explicit_client_is_rejected_without_fallback() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, None);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The named client does not exist; the valid issuer is NOT used instead.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(pane_id),
            client_id: Some(ClientId::new()),
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane(tab_id),
        None
    );
}

#[test]
fn focus_from_a_clientless_source_defaults_to_the_sole_client() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (focused_pane_id, target_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, focused_pane_id);
    register_pane_record(&mut session, target_pane_id);
    register_session_tab(&mut session, tab_id, focused_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(focused_pane_id),
                LayoutNode::Pane(target_pane_id),
            ],
        )));
    attach_client(&mut session, client_id, tab_id, Some(focused_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(target_pane_id),
            client_id: None,
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(list_event_names(&emitted_events), ["PaneFocused"]);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane(tab_id),
        Some(target_pane_id)
    );
}

#[test]
fn focus_from_a_clientless_source_with_two_clients_is_ambiguous() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, ClientId::new(), tab_id, None);
    attach_client(&mut session, ClientId::new(), tab_id, None);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(pane_id),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some("several clients are attached; name the target client".to_string()),
        }
    );
}

#[test]
fn focus_with_no_attached_client_at_all_is_stale() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(pane_id),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: Some("no client is attached to the session".to_string()),
        }
    );
}

#[test]
fn focus_an_exited_pane_succeeds() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (first_pane_id, exited_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, exited_pane_id);
    register_session_tab(&mut session, tab_id, first_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(first_pane_id),
                LayoutNode::Pane(exited_pane_id),
            ],
        )));
    // A dead pane is a visible, focusable placeholder until it is removed.
    let exited_at = SystemTime::now();
    {
        let pane_record = session
            .panes
            .get_pane_record_mut_by_id(exited_pane_id)
            .expect("pane record");
        let _ = pane_record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);
        let _ = pane_record.update_lifecycle(PaneLifecycleEvent::ProcessExited {
            exit_code: Some(0),
            exited_at,
        });
        assert_eq!(
            *pane_record.get_lifecycle(),
            PaneLifecycle::Exited {
                exit_code: Some(0),
                exited_at,
            }
        );
    }
    attach_client(&mut session, client_id, tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(exited_pane_id),
            client_id: None,
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(list_event_names(&emitted_events), ["PaneFocused"]);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane(tab_id),
        Some(exited_pane_id)
    );
}

#[test]
fn focus_activation_reflows_the_expanded_member_pty() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Split off a live pane, then stack a new pane onto it. The stack has the
    // original pane active and the earlier pane collapsed to a header.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { .. } => {}
        other => panic!("expected Ok, got {other:?}"),
    }
    let stacked_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            should_stack: true,
            ..build_new_pane_args()
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { .. } => {}
        other => panic!("expected Ok, got {other:?}"),
    }
    let third_pane_id = {
        let mut remaining_pane_ids = runtime.session_by_id[&session_id]
            .panes
            .list_pane_records()
            .map(PaneRecord::get_pane_id)
            .filter(|pane_id| *pane_id != root_pane_id && *pane_id != stacked_pane_id);
        let new_pane_id = remaining_pane_ids.next().expect("a third pane exists");
        assert!(remaining_pane_ids.next().is_none(), "exactly one new pane");
        new_pane_id
    };
    let third_pane_resize_count_before = fake_pty_backend
        .list_pane_sizes(third_pane_id)
        .expect("third pane spawned")
        .len();

    // Focusing the collapsed stacked pane expands it: 80x24 leaves an 80x22 pane region;
    // the half-width stack member has one header, so content becomes 38x19.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(stacked_pane_id),
            client_id: None,
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => {
            // LayoutChanged + PtyResized(stacked_pane_id) + PaneFocused. The third pane collapses to a
            // header and keeps its last PTY size, so it is not resized.
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PtyResized", "PaneFocused"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(root_pane_id),
                LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                    vec![stacked_pane_id, third_pane_id],
                    0,
                )),
            ],
        ))
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane(tab_id),
        Some(stacked_pane_id)
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(stacked_pane_id)
            .expect("stacked pane spawned")
            .last()
            .copied(),
        Some(PtySize {
            column_count: 38,
            row_count: 19
        })
    );
    // The newly collapsed third pane keeps its last PTY size: no resize reached it.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(third_pane_id)
            .expect("third pane spawned")
            .len(),
        third_pane_resize_count_before
    );
}

#[test]
fn in_session_cli_source_pane_in_another_session_is_gone() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let session_a = SessionId::new();
    let pane_id_in_b = PaneId::new();

    let mut claimed_session_state = build_bare_session(session_a);
    attach_client(&mut claimed_session_state, client_id, TabId::new(), None);
    runtime
        .session_by_id
        .insert(claimed_session_state.session_id, claimed_session_state);

    // The pane lives in a *different* session; the acting session has no such
    // source pane, so the command is refused before any target resolution.
    let mut other_session_state = build_bare_session(SessionId::new());
    register_pane_record(&mut other_session_state, pane_id_in_b);
    runtime
        .session_by_id
        .insert(other_session_state.session_id, other_session_state);

    let command_source = CommandSource::from_in_session_cli(
        session_a,
        Some(client_id),
        pane_id_in_b,
        PathBuf::from("/sock"),
    );
    let command_envelope =
        build_command_envelope(command_source, Command::ClosePane(ClosePaneArgs::default()));
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetGone,
            help: Some("source pane no longer exists".to_string()),
        }
    );
}

#[test]
fn in_session_cli_pane_command_without_a_client_succeeds() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(session_id);
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    runtime.session_by_id.insert(session_id, session);

    // Grow a second pane; the split focuses it for the attached client.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);

    // The issuing pane was spawned with no designated client. Closing it is
    // pane-scoped, so no client is needed: the pane closes, and the attached
    // client's focus falls back to the root — PaneClosing + PaneRemoved +
    // LayoutChanged + PaneFocused.
    let command_source =
        CommandSource::from_in_session_cli(session_id, None, new_pane_id, PathBuf::from("/sock"));
    let command_envelope =
        build_command_envelope(command_source, Command::ClosePane(ClosePaneArgs::default()));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .is_none());
}

#[test]
fn in_session_cli_pane_command_with_a_detached_client_succeeds() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(session_id);
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);

    // The client that spawned the pane is long gone (never attached here).
    // The pane outlives it: a pane-scoped command from that pane still works.
    let stranger_client_id = ClientId::new();
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(stranger_client_id),
        new_pane_id,
        PathBuf::from("/sock"),
    );
    let command_envelope =
        build_command_envelope(command_source, Command::ClosePane(ClosePaneArgs::default()));
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .is_none());
}

#[test]
fn in_session_cli_client_scoped_with_no_attached_client_is_stale() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(session_id);
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    runtime.session_by_id.insert(session_id, session);

    // Lock mode is one client's own state, and no client is attached to stand
    // in for the one this pane never had.
    let command_source =
        CommandSource::from_in_session_cli(session_id, None, root_pane_id, PathBuf::from("/sock"));
    let command_envelope = build_command_envelope(
        command_source,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: Some("no client is attached to the session".to_string()),
        }
    );
}

#[test]
fn in_session_cli_from_a_closing_pane_is_gone() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(session_id);
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    // The pane is mid-teardown: a command issued from it must not steer the
    // session anymore.
    session
        .panes
        .get_pane_record_mut_by_id(root_pane_id)
        .expect("pane record")
        .update_lifecycle(PaneLifecycleEvent::CloseRequested {
            close_requested_at: SystemTime::now(),
        })
        .expect("spawning pane accepts a close request");
    runtime.session_by_id.insert(session_id, session);

    let command_source =
        CommandSource::from_in_session_cli(session_id, None, root_pane_id, PathBuf::from("/sock"));
    let command_envelope = build_command_envelope(
        command_source,
        Command::MoveTab(MoveTabArgs {
            tab_id: None,
            target_tab_index: 0,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetGone,
            help: Some("source pane no longer exists".to_string()),
        }
    );
}

#[test]
fn in_session_cli_from_an_exited_pane_is_a_valid_source() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(session_id);
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    // The pane's child ran and exited; its close policy keeps it on screen.
    // A background child it left behind can still command from it.
    {
        let pane_record = session
            .panes
            .get_pane_record_mut_by_id(root_pane_id)
            .expect("pane record");
        pane_record
            .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
            .expect("spawning pane starts");
        pane_record
            .update_lifecycle(PaneLifecycleEvent::ProcessExited {
                exit_code: Some(0),
                exited_at: SystemTime::now(),
            })
            .expect("running pane exits");
    }
    runtime.session_by_id.insert(session_id, session);

    let command_source =
        CommandSource::from_in_session_cli(session_id, None, root_pane_id, PathBuf::from("/sock"));
    let command_envelope = build_command_envelope(
        command_source,
        Command::MoveTab(MoveTabArgs {
            tab_id: None,
            target_tab_index: 0,
        }),
    );
    let command_id = command_envelope.command_id;
    // The tab already sits at slot 0, so the move applies with no events.
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
}

#[test]
fn mouse_select_cannot_be_issued_from_the_cli() {
    // The CLI has no mouse-select verb; the command is refused before any
    // state is read, so even an empty runtime answers.
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let command_source = CommandSource::from_in_session_cli(
        SessionId::new(),
        Some(ClientId::new()),
        PaneId::new(),
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(command_source, Command::ToggleMouseSelect);
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::Unauthorized,
            help: Some("command cannot be issued from the CLI".to_string()),
        }
    );
}

#[test]
fn mouse_select_is_refused_from_the_source_a_control_connection_stamps() {
    // A control connection that presented `CommandSource::Internal` reaches
    // dispatch as `ExternalCli { session_id: None, target_client: None }`, so
    // the CLI-admission check refuses the verb the CLI has no word for.
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(None, None),
        Command::ToggleMouseSelect,
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::Unauthorized,
            help: Some("command cannot be issued from the CLI".to_string()),
        }
    );
}

#[test]
fn copy_cannot_be_issued_from_the_cli() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let command_source = CommandSource::from_in_session_cli(
        SessionId::new(),
        Some(ClientId::new()),
        PaneId::new(),
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(
        command_source,
        Command::Visual(VisualCommand::Copy(CopyArgs {
            pane_id: PaneId::new(),
            should_trim_trailing_whitespace: true,
            clipboard_target: CopyTarget::Osc52,
        })),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::Unauthorized,
            help: Some("command cannot be issued from the CLI".to_string()),
        }
    );
}

#[test]
fn quit_cannot_be_issued_from_inside_a_pane() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let command_source = CommandSource::from_in_session_cli(
        SessionId::new(),
        Some(ClientId::new()),
        PaneId::new(),
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(command_source, Command::Quit);
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::Unauthorized,
            help: Some("command cannot be issued from the CLI".to_string()),
        }
    );
    assert!(!runtime.is_quit_requested());
}

#[test]
fn quit_from_an_external_cli_is_accepted() {
    // `kill-session` sends `Quit` from outside the session. It names no client,
    // so it ends the process whatever `auto-close-session` says.
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let command_source = CommandSource::from_external_cli(None, None);
    let command_envelope = build_command_envelope(command_source, Command::Quit);
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
    assert!(runtime.is_quit_requested());
    assert!(runtime.should_shutdown_immediately);
}

#[test]
fn in_session_cli_with_an_unknown_session_is_not_found() {
    // The build_internal_command_envelope names a session this runtime does not run.
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let command_source = CommandSource::from_in_session_cli(
        SessionId::new(),
        Some(ClientId::new()),
        PaneId::new(),
        PathBuf::from("/sock"),
    );
    let command_envelope =
        build_command_envelope(command_source, Command::ClosePane(ClosePaneArgs::default()));
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
}

#[test]
fn external_cli_default_pane_with_no_attached_client_is_stale() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let session_id = SessionId::new();
    runtime
        .session_by_id
        .insert(session_id, build_bare_session(session_id));

    // A session resolves, but its focused-pane default acts through the
    // acting client, and a session with nobody attached has none.
    let command_source = CommandSource::from_external_cli(Some(session_id), None);
    let command_envelope =
        build_command_envelope(command_source, Command::ClosePane(ClosePaneArgs::default()));
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: Some("no client is attached to the session".to_string()),
        }
    );
}

#[test]
fn focused_default_outside_active_tab_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let active_tab_pane_id = PaneId::new();
    let focused_elsewhere = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, active_tab_pane_id);
    // `focused_elsewhere` is in the registry but not in the active tab's layout.
    register_pane_record(&mut session, focused_elsewhere);
    register_session_tab(&mut session, tab_id, active_tab_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(focused_elsewhere));
    runtime.session_by_id.insert(session.session_id, session);

    // Fullscreen defaults through the focused pane; it is outside the tab.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::TogglePaneFullscreen,
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("pane not in the client's active tab".to_string()),
        }
    );
}

#[test]
fn run_command_pane_requires_a_pane_anchor() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let session_id = SessionId::new();
    runtime
        .session_by_id
        .insert(session_id, build_bare_session(session_id));

    // A session alone is not enough — RunCommandPane splits the acting
    // client's focused pane, like NewPane, and a session with nobody
    // attached has no acting client.
    let command_source = CommandSource::from_external_cli(Some(session_id), None);
    let command_envelope = build_command_envelope(
        command_source,
        Command::RunCommandPane(RunCommandPaneArgs {
            spawn_spec: build_spawn_spec(),
            working_directory: None,
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Right,
            should_stack: false,
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: Some("no client is attached to the session".to_string()),
        }
    );
}

#[test]
fn run_command_pane_spawns_and_records_the_command() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Splits the focused pane and spawns the requested command in the new pane.
    match runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::RunCommandPane(RunCommandPaneArgs {
            spawn_spec: build_spawn_spec(),
            working_directory: None,
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Right,
            should_stack: false,
            client_id: None,
        }),
    )) {
        CommandResult::Ok { .. } => {}
        other => panic!("expected Ok, got {other:?}"),
    }

    // The command is spawned verbatim — save for koshi's terminal identity
    // and the in-session identity vars added to its environment — and
    // recorded on the pane without the identity vars, taking the default
    // close-on-exit policy.
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    let mut recorded_spawn_spec = build_spawn_spec();
    recorded_spawn_spec.environment_variables =
        runtime.apply_terminal_identity_environment_variables(BTreeMap::new());
    let mut launched_spawn_spec = recorded_spawn_spec.clone();
    launched_spawn_spec
        .environment_variables
        .extend(build_koshi_environment(
            session_id,
            Some(client_id),
            new_pane_id,
            koshi_paths::resolve_runtime_directory().as_deref(),
        ));
    assert_eq!(
        fake_pty_backend.get_spawn_spec(new_pane_id).unwrap(),
        launched_spawn_spec
    );
    let pane_record = runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .unwrap();
    assert_eq!(pane_record.spawn_spec, Some(recorded_spawn_spec));
    assert_eq!(pane_record.exit_policy, PaneExitPolicy::CloseOnExit);
    // The new command pane is focused for the issuing client.
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(new_pane_id)
    );
}

#[test]
fn run_command_pane_carries_cwd_into_the_command() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The command carries no cwd of its own, so the command_args `cwd` fills it in.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::RunCommandPane(RunCommandPaneArgs {
            spawn_spec: build_spawn_spec(),
            working_directory: Some(PathBuf::from("/work")),
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Right,
            should_stack: false,
            client_id: None,
        }),
    ));

    // `--cwd` reaches the spawned child and is recorded as the pane's directory.
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    assert_eq!(
        fake_pty_backend
            .get_spawn_spec(new_pane_id)
            .unwrap()
            .working_directory,
        Some(PathBuf::from("/work"))
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(new_pane_id)
            .unwrap()
            .working_directory,
        Some(PathBuf::from("/work"))
    );
}

#[test]
fn run_command_pane_args_carry_placement_into_the_new_pane_mapping() {
    // The run → new-pane mapping forwards the source pane and every
    // placement field verbatim; only the command is made mandatory.
    let source_pane_id = PaneId::new();
    let tab_id = TabId::new();
    let client_id = ClientId::new();
    let command_args = RunCommandPaneArgs {
        spawn_spec: build_spawn_spec(),
        working_directory: Some(PathBuf::from("/work")),
        source_pane_id: Some(source_pane_id),
        tab_id: Some(tab_id),
        direction: Direction::Down,
        should_stack: true,
        client_id: Some(client_id),
    };
    assert_eq!(
        Server::run_command_new_pane_args(&command_args),
        NewPaneArgs {
            source_pane_id: Some(source_pane_id),
            tab_id: Some(tab_id),
            direction: Direction::Down,
            should_stack: true,
            working_directory: Some(PathBuf::from("/work")),
            spawn_spec: Some(build_spawn_spec()),
            client_id: Some(client_id),
        }
    );
}

#[test]
fn in_session_cli_session_id_is_authoritative_over_a_mismatched_client() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let claimed_session = SessionId::new();
    let source_pane_id = PaneId::new();
    let tab_id = TabId::new();

    // The build_internal_command_envelope claims one session, but the client is attached to another
    // session. The claimed session is looked up by its id, and the
    // client-scoped command checks the client there before it acts.
    let mut claimed_session_state = build_bare_session(claimed_session);
    register_pane_record(&mut claimed_session_state, source_pane_id);
    register_session_tab(&mut claimed_session_state, tab_id, source_pane_id);
    runtime
        .session_by_id
        .insert(claimed_session, claimed_session_state);
    let mut attached_session_state = build_bare_session(SessionId::new());
    attach_client(&mut attached_session_state, client_id, TabId::new(), None);
    runtime
        .session_by_id
        .insert(attached_session_state.session_id, attached_session_state);

    let command_source = CommandSource::from_in_session_cli(
        claimed_session,
        Some(client_id),
        source_pane_id,
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(
        command_source,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: Some("no client is attached to the session".to_string()),
        }
    );
}

#[test]
fn focus_tab_next_in_a_single_tab_session_is_a_clean_noop() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, None);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Next wraps the single tab back onto itself: the target resolves to the
    // already-active tab, so nothing changes and no events are emitted.
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Next,
            client_id: None,
        }),
    ));
    match command_result {
        CommandResult::Ok { emitted_events, .. } => assert!(emitted_events.is_empty()),
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        tab_id
    );
}

#[test]
fn focus_tab_relative_with_a_stale_active_tab_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let live_tab_id = TabId::new();
    let stale_tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    // The session has a real tab, but the client's active tab points elsewhere.
    register_session_tab(&mut session, live_tab_id, pane_id);
    attach_client(&mut session, client_id, stale_tab_id, None);
    runtime.session_by_id.insert(session.session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Prev,
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
}

#[test]
fn focus_target_with_removed_registry_record_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    // Pane is in the tab's layout but NOT in the registry (pane record removed).
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    runtime.session_by_id.insert(session.session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(pane_id),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
}

#[test]
fn in_session_cli_tab_default_uses_the_source_pane_tab() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(session_id);
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    // Client's active tab is B, but the CLI command was issued from tab A's pane.
    attach_client(&mut session, client_id, tab_id_b, None);
    runtime.session_by_id.insert(session.session_id, session);

    // CloseTab with no explicit tab — InSessionCli resolves via the tab
    // containing pane_a (tab A), not the client's active tab (tab B).
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(client_id),
        pane_id_a,
        PathBuf::from("/sock"),
    );
    let command_result = runtime.dispatch(build_command_envelope(
        command_source,
        Command::CloseTab(CloseTabArgs::default()),
    ));
    assert!(matches!(command_result, CommandResult::Ok { .. }));

    // Tab A — the source pane's tab — was closed; the client's active tab B
    // is untouched.
    let session = &runtime.session_by_id[&session_id];
    assert!(!session.tabs.contains_key(&tab_id_a));
    assert!(session.tabs.contains_key(&tab_id_b));
    assert!(session.panes.get_pane_record_by_id(pane_id_a).is_none());
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_b
    );
}

#[test]
fn in_session_cli_tab_default_with_removed_source_pane_is_gone() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(session_id);
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, None);
    runtime.session_by_id.insert(session.session_id, session);

    // The source pane_id doesn't exist in the registry (nor any tab layout).
    let stale_pane_id = PaneId::new();
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(client_id),
        stale_pane_id,
        PathBuf::from("/sock"),
    );
    let command_envelope =
        build_command_envelope(command_source, Command::CloseTab(CloseTabArgs::default()));
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetGone,
            help: Some("source pane no longer exists".to_string()),
        }
    );
}

#[test]
fn new_pane_spawns_and_runs_the_child() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    let command_id = command_envelope.command_id;
    let command_result = runtime.dispatch(command_envelope);

    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    assert!(
        matches!(command_result, CommandResult::Ok { command_id: applied_command_id, .. } if applied_command_id == command_id)
    );
    // The child was spawned under the pane's own id and advanced to Running.
    assert_eq!(fake_pty_backend.list_spawned_pane_ids(), vec![new_pane_id]);
    assert_eq!(
        *runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(new_pane_id)
            .unwrap()
            .get_lifecycle(),
        PaneLifecycle::Running
    );
    // Its handle is parked so the reader thread keeps feeding output.
    assert!(runtime.pty_handle_by_pane_id.contains_key(&new_pane_id));
    // The root has no PTY yet, so it is neither spawned nor parked.
    assert!(!runtime.pty_handle_by_pane_id.contains_key(&root_pane_id));
}

#[test]
fn new_pane_without_command_spawns_the_default_shell() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));

    // command: None resolves to the platform default shell carrying koshi's
    // terminal identity and the in-session identity vars in its environment.
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    let mut expected_spawn_spec = runtime.build_default_shell_spec(None, BTreeMap::new());
    expected_spawn_spec
        .environment_variables
        .extend(build_koshi_environment(
            session_id,
            Some(client_id),
            new_pane_id,
            koshi_paths::resolve_runtime_directory().as_deref(),
        ));
    assert_eq!(
        fake_pty_backend.get_spawn_spec(new_pane_id).unwrap(),
        expected_spawn_spec
    );
}

#[test]
fn new_pane_with_command_spawns_that_command() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            spawn_spec: Some(build_spawn_spec()),
            ..build_new_pane_args()
        }),
    ));

    // An explicit command is spawned verbatim, save for koshi's terminal
    // identity and the in-session identity vars added to its environment.
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    let mut expected_spawn_spec = build_spawn_spec();
    expected_spawn_spec.environment_variables =
        runtime.apply_terminal_identity_environment_variables(BTreeMap::new());
    expected_spawn_spec
        .environment_variables
        .extend(build_koshi_environment(
            session_id,
            Some(client_id),
            new_pane_id,
            koshi_paths::resolve_runtime_directory().as_deref(),
        ));
    assert_eq!(
        fake_pty_backend.get_spawn_spec(new_pane_id).unwrap(),
        expected_spawn_spec
    );
}

#[test]
fn new_pane_spawn_failure_leaves_no_trace() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    fake_pty_backend.fail_spawns_with(PtyError::Spawn {
        detail: "boom".to_string(),
    });
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    let before_layout = runtime.session_by_id[&session_id].tabs[&tab_id]
        .get_layout_tree()
        .clone();

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    let command_id = command_envelope.command_id;

    // The child cannot launch, so the command rejects and nothing is committed.
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("failed to launch the pane's process".to_string()),
        }
    );
    // No new pane, the layout is untouched, no handle was parked, and the
    // client's focus never moved to a pane that never existed.
    assert_eq!(
        runtime.session_by_id[&session_id].panes.pane_record_count(),
        1
    );
    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(root_pane_id)
        .is_some());
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &before_layout
    );
    assert!(runtime.pty_handle_by_pane_id.is_empty());
    assert!(fake_pty_backend.list_spawned_pane_ids().is_empty());
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(root_pane_id)
    );
}

#[test]
fn new_pane_adoption_spawn_failure_leaves_no_trace() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    fake_pty_backend.fail_spawns_with(PtyError::Spawn {
        detail: "boom".to_string(),
    });
    let client_id = ClientId::new();
    let front_tab_id = TabId::new();
    let back_tab_id = TabId::new();
    let front_pane_id = PaneId::new();
    let back_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_pane_id);
    register_pane_record(&mut session, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);
    attach_client(&mut session, client_id, front_tab_id, Some(front_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    let before_layout = runtime.session_by_id[&session_id].tabs[&back_tab_id]
        .get_layout_tree()
        .clone();

    // The split would adopt the client onto the background tab, but the spawn
    // happens first and fails — so the adoption never occurs: the client stays on
    // the front tab and no pane appears.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(back_pane_id),
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("failed to launch the pane's process".to_string()),
        }
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        front_tab_id
    );
    assert_eq!(session.panes.pane_record_count(), 2);
    assert_eq!(session.tabs[&back_tab_id].get_layout_tree(), &before_layout);
    assert!(runtime.pty_handle_by_pane_id.is_empty());
    assert!(fake_pty_backend.list_spawned_pane_ids().is_empty());
}

#[test]
fn new_pane_on_a_background_tab_adopts_a_viewer() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();

    // One session, one client viewing the front tab. A second, background tab
    // holds `pane_back` and has no viewer, so it has no viewport of its own.
    let client_id = ClientId::new();
    let front_tab_id = TabId::new();
    let back_tab_id = TabId::new();
    let front_pane_id = PaneId::new();
    let back_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_pane_id);
    register_pane_record(&mut session, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);
    attach_client(&mut session, client_id, front_tab_id, Some(front_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(back_pane_id),
            ..build_new_pane_args()
        }),
    ));

    // No client was viewing the background tab, so the sole client is adopted
    // onto it: it switches to view that tab, the split spawns like any in-view
    // one, and the adopted client focuses the new pane. Events: TabFocused,
    // PaneCreated, LayoutChanged, PaneFocused, PtyResized (the PTY-less
    // `pane_back` sibling is skipped by the reflow).
    let new_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != front_pane_id && *pane_id != back_pane_id)
        .expect("the freshly split pane");
    match command_result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(
            list_event_names(&emitted_events),
            [
                "TabFocused",
                "PaneCreated",
                "LayoutChanged",
                "PaneFocused",
                "PtyResized"
            ]
        ),
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        back_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(back_tab_id),
        Some(new_pane_id)
    );
    assert!(runtime.pty_handle_by_pane_id.contains_key(&new_pane_id));
    assert!(fake_pty_backend
        .list_spawned_pane_ids()
        .contains(&new_pane_id));
    assert_eq!(
        *runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(new_pane_id)
            .unwrap()
            .get_lifecycle(),
        PaneLifecycle::Running
    );
}

#[test]
fn new_pane_on_a_background_tab_adopts_the_issuing_client() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();

    // Two clients in one session, both on the front tab. The issuer is chosen as
    // the *higher*-id client, so the earliest-id fallback (`.min()`) would pick
    // the other one — proving adoption prefers the issuer, not the lowest id.
    let first_client_id = ClientId::new();
    let second_client_id = ClientId::new();
    let (issuer_client_id, bystander_client_id) = if first_client_id < second_client_id {
        (second_client_id, first_client_id)
    } else {
        (first_client_id, second_client_id)
    };
    let front_tab_id = TabId::new();
    let back_tab_id = TabId::new();
    let front_pane_id = PaneId::new();
    let back_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_pane_id);
    register_pane_record(&mut session, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);
    attach_client(
        &mut session,
        bystander_client_id,
        front_tab_id,
        Some(front_pane_id),
    );
    attach_client(
        &mut session,
        issuer_client_id,
        front_tab_id,
        Some(front_pane_id),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The issuer splits the background tab; it — not the lower-id bystander — is
    // pulled onto the tab and focuses the new pane.
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(issuer_client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(back_pane_id),
            ..build_new_pane_args()
        }),
    ));
    // TabFocused, PaneCreated, LayoutChanged, PaneFocused, PtyResized.
    match command_result {
        CommandResult::Ok { emitted_events, .. } => assert_eq!(
            list_event_names(&emitted_events),
            [
                "TabFocused",
                "PaneCreated",
                "LayoutChanged",
                "PaneFocused",
                "PtyResized"
            ]
        ),
        other => panic!("expected Ok, got {other:?}"),
    }

    let new_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != front_pane_id && *pane_id != back_pane_id)
        .expect("the freshly split pane");
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(issuer_client_id)
            .unwrap()
            .get_active_tab(),
        back_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(issuer_client_id)
            .unwrap()
            .get_focused_pane(back_tab_id),
        Some(new_pane_id)
    );
    // The bystander was left exactly where it was.
    assert_eq!(
        session
            .clients
            .get_client_by_id(bystander_client_id)
            .unwrap()
            .get_active_tab(),
        front_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(bystander_client_id)
            .unwrap()
            .get_focused_pane(back_tab_id),
        None
    );
    assert!(runtime.pty_handle_by_pane_id.contains_key(&new_pane_id));
}

#[test]
fn new_pane_external_multiple_clients_is_ambiguous() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    let front_tab_id = TabId::new();
    let back_tab_id = TabId::new();
    let front_pane_id = PaneId::new();
    let back_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_pane_id);
    register_pane_record(&mut session, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);
    let first_client_id = ClientId::new();
    let second_client_id = ClientId::new();
    attach_client(
        &mut session,
        first_client_id,
        front_tab_id,
        Some(front_pane_id),
    );
    attach_client(
        &mut session,
        second_client_id,
        front_tab_id,
        Some(front_pane_id),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // No issuing client (external) and the background tab has no viewer, but the
    // session has two attached clients — adopting either would hijack a bystander,
    // so it rejects and asks for a named target, changing nothing.
    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(back_pane_id),
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some("multiple clients; name a target client for the new pane".to_string()),
        }
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.panes.pane_record_count(), 2);
    assert_eq!(
        session
            .clients
            .get_client_by_id(first_client_id)
            .unwrap()
            .get_active_tab(),
        front_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(second_client_id)
            .unwrap()
            .get_active_tab(),
        front_tab_id
    );
}

#[test]
fn new_pane_external_targets_a_named_client() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    let front_tab_id = TabId::new();
    let back_tab_id = TabId::new();
    let front_pane_id = PaneId::new();
    let back_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_pane_id);
    register_pane_record(&mut session, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);
    let bystander_client_id = ClientId::new();
    let target_client_id = ClientId::new();
    attach_client(
        &mut session,
        bystander_client_id,
        front_tab_id,
        Some(front_pane_id),
    );
    attach_client(
        &mut session,
        target_client_id,
        front_tab_id,
        Some(front_pane_id),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // External CLI names `target`: it is adopted onto the background tab and
    // focuses the new pane; the bystander is left untouched.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(back_pane_id),
            client_id: Some(target_client_id),
            ..build_new_pane_args()
        }),
    ));
    let new_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != front_pane_id && *pane_id != back_pane_id)
        .expect("the freshly split pane");
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(target_client_id)
            .unwrap()
            .get_active_tab(),
        back_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(target_client_id)
            .unwrap()
            .get_focused_pane(back_tab_id),
        Some(new_pane_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(bystander_client_id)
            .unwrap()
            .get_active_tab(),
        front_tab_id
    );
    assert!(runtime.pty_handle_by_pane_id.contains_key(&new_pane_id));
}

#[test]
fn new_pane_external_unattached_target_client_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    let front_tab_id = TabId::new();
    let back_tab_id = TabId::new();
    let front_pane_id = PaneId::new();
    let back_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_pane_id);
    register_pane_record(&mut session, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);
    let client_id = ClientId::new();
    attach_client(&mut session, client_id, front_tab_id, Some(front_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The named client is not attached to the session: reject before mutating.
    let unattached_client_id = ClientId::new();
    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(back_pane_id),
            client_id: Some(unattached_client_id),
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id].panes.pane_record_count(),
        2
    );
}

#[test]
fn new_pane_explicit_client_wins_over_the_in_session_issuer() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let issuer_client_id = ClientId::new();
    let other_client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, issuer_client_id, tab_id, Some(root_pane_id));
    attach_client(&mut session, other_client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The issuer runs the command but names `other` as the target client: the
    // explicit `--client` wins even in-session, so `other` focuses the new pane
    // and the issuer's focus is left untouched.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(issuer_client_id),
        Command::NewPane(NewPaneArgs {
            client_id: Some(other_client_id),
            ..build_new_pane_args()
        }),
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(other_client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(new_pane_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(issuer_client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(root_pane_id)
    );
}

#[test]
fn new_pane_explicit_unattached_client_is_rejected_even_with_an_issuer() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let issuer_client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, issuer_client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // A wrong `--client` (not attached) rejects outright — no fallback to the
    // issuing client, even though one is present.
    let unattached_client_id = ClientId::new();
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(issuer_client_id),
        Command::NewPane(NewPaneArgs {
            client_id: Some(unattached_client_id),
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id].panes.pane_record_count(),
        1
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(issuer_client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(root_pane_id)
    );
}

#[test]
fn new_pane_wont_fit_on_a_background_tab_changes_nothing() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();

    // One client viewing the front tab at a 2x1 viewport too small to split.
    let client_id = ClientId::new();
    let front_tab_id = TabId::new();
    let back_tab_id = TabId::new();
    let front_pane_id = PaneId::new();
    let back_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_pane_id);
    register_pane_record(&mut session, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);
    let mut client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 2,
            row_count: 1,
        },
        None,
        front_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(front_tab_id, front_pane_id);
    session.attach_client(client);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The split would size against the 2x1 viewport of the client that would be
    // adopted, but fit is checked before anything mutates: it cannot fit, so the
    // command rejects and nothing changes — the client is never moved.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(back_pane_id),
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("not enough space for a new pane".to_string()),
        }
    );
    let session = &runtime.session_by_id[&session_id];
    // The client never left its tab, nothing spawned, no pane added.
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        front_tab_id
    );
    assert_eq!(session.panes.pane_record_count(), 2);
    assert!(fake_pty_backend.list_spawned_pane_ids().is_empty());
    assert!(runtime.pty_handle_by_pane_id.is_empty());
}

#[test]
fn new_pane_adoption_reflows_the_vacated_tab() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();

    let front_tab_id = TabId::new();
    let back_tab_id = TabId::new();
    let front_pane_id = PaneId::new();
    let back_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_pane_id);
    register_pane_record(&mut session, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);

    // The issuer is the smaller, size-constraining viewer of the front tab; the
    // other client is the larger 80-wide viewer. Both view the front tab.
    let narrow_viewer_client_id = ClientId::new();
    let mut narrow_viewer = Client::from_attachment(
        narrow_viewer_client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 40,
            row_count: 10,
        },
        None,
        front_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    narrow_viewer.update_focused_pane(front_tab_id, front_pane_id);
    session.attach_client(narrow_viewer);
    let wide_viewer_client_id = ClientId::new();
    attach_client(
        &mut session,
        wide_viewer_client_id,
        front_tab_id,
        Some(front_pane_id),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Split the front tab (from B) so it holds a live PTY sized to A's 40-wide
    // constraint.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(wide_viewer_client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let split_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != front_pane_id && *pane_id != back_pane_id)
        .expect("the front-tab split pane");
    let pane_size_history_before = fake_pty_backend.list_pane_sizes(split_pane_id).unwrap();

    // The narrow viewer issues a split against the background tab: it is adopted
    // onto it and leaves the front tab, whose viewport now grows to 80 wide. The front tab's
    // live PTY is reflowed exactly once, larger than before.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(narrow_viewer_client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(back_pane_id),
            ..build_new_pane_args()
        }),
    ));
    let pane_size_history_after = fake_pty_backend.list_pane_sizes(split_pane_id).unwrap();
    assert_eq!(
        pane_size_history_after.len(),
        pane_size_history_before.len() + 1
    );
    assert!(
        pane_size_history_after.last().unwrap().column_count
            > pane_size_history_before.last().unwrap().column_count
    );
}

#[test]
fn new_pane_adoption_vacated_tab_with_no_viewer_keeps_sizes() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let front_tab_id = TabId::new();
    let back_tab_id = TabId::new();
    let front_pane_id = PaneId::new();
    let back_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_pane_id);
    register_pane_record(&mut session, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);
    attach_client(&mut session, client_id, front_tab_id, Some(front_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Give the front tab a live PTY pane by splitting it while viewed.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let split_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != front_pane_id && *pane_id != back_pane_id)
        .expect("the front-tab split pane");
    let resize_count_before = fake_pty_backend
        .list_pane_sizes(split_pane_id)
        .unwrap()
        .len();

    // The sole viewer is adopted onto the background tab, leaving the front tab
    // with no viewer: its live PTY keeps its size — not resized at all.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(back_pane_id),
            ..build_new_pane_args()
        }),
    ));
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        back_tab_id
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .len(),
        resize_count_before
    );
}

#[test]
fn new_pane_adoption_reflows_a_stale_sized_background_sibling() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let back_tab_id = TabId::new();
    let front_tab_id = TabId::new();
    let back_pane_id = PaneId::new();
    let front_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, back_pane_id);
    register_pane_record(&mut session, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    // One client (40 wide) views the back tab; the other (100 wide) views the front.
    let narrow_viewer_client_id = ClientId::new();
    let mut narrow_viewer = Client::from_attachment(
        narrow_viewer_client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 40,
            row_count: 10,
        },
        None,
        back_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    narrow_viewer.update_focused_pane(back_tab_id, back_pane_id);
    session.attach_client(narrow_viewer);
    let wide_viewer_client_id = ClientId::new();
    let mut wide_viewer = Client::from_attachment(
        wide_viewer_client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 100,
            row_count: 50,
        },
        None,
        front_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    wide_viewer.update_focused_pane(front_tab_id, front_pane_id);
    session.attach_client(wide_viewer);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The narrow viewer splits the back tab while it is the only 40-wide viewer: the sibling's
    // PTY is sized to 40 wide.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(narrow_viewer_client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let split_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != back_pane_id && *pane_id != front_pane_id)
        .expect("the back-tab split pane");
    let narrow_pane_size = *fake_pty_backend
        .list_pane_sizes(split_pane_id)
        .unwrap()
        .last()
        .unwrap();

    // The narrow viewer leaves the back tab, which is now unviewed; its PTYs keep
    // the stale 40-wide size.
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .clients
        .get_client_mut_by_id(narrow_viewer_client_id)
        .unwrap()
        .update_active_tab(front_tab_id);

    // The wide viewer splits `pane_back` on the now-background tab and is adopted
    // at 100 wide. The untouched sibling `split_pane_id` must be reflowed to the larger geometry, not left
    // at its stale 40-wide size.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(wide_viewer_client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(back_pane_id),
            ..build_new_pane_args()
        }),
    ));
    let wide_pane_size = *fake_pty_backend
        .list_pane_sizes(split_pane_id)
        .unwrap()
        .last()
        .unwrap();
    assert!(
        wide_pane_size.column_count > narrow_pane_size.column_count,
        "stale background sibling was reflowed to the larger viewport (was {narrow_pane_size:?}, now {wide_pane_size:?})"
    );
}

#[test]
fn new_pane_external_sole_client_that_cannot_fit_is_min_size() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let front_tab_id = TabId::new();
    let back_tab_id = TabId::new();
    let front_pane_id = PaneId::new();
    let back_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_pane_id);
    register_pane_record(&mut session, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);
    // The session's sole client is too small (2x1) to hold a split.
    let client_id = ClientId::new();
    let mut client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 2,
            row_count: 1,
        },
        None,
        front_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(front_tab_id, front_pane_id);
    session.attach_client(client);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // An external CLI (no issuer) targets the unviewed background tab: the sole
    // client is the unambiguous default, but its viewport cannot hold the split,
    // so it rejects MinSize — before any mutation.
    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(back_pane_id),
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("not enough space for a new pane".to_string()),
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id].panes.pane_record_count(),
        2
    );
}

#[test]
fn new_pane_leaves_an_unchanged_sibling_pty_alone() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Split 1 creates a sibling and focuses it. Split 2 splits that sibling and
    // focuses its new child.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let first_split_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != root_pane_id)
        .expect("first split pane");
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let second_split_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != root_pane_id && *pane_id != first_split_pane_id)
        .expect("second split pane");
    let first_pane_resize_count_before = fake_pty_backend
        .list_pane_sizes(first_split_pane_id)
        .unwrap()
        .len();
    let second_pane_resize_count_before = fake_pty_backend
        .list_pane_sizes(second_split_pane_id)
        .unwrap()
        .len();

    // Split 3 nests another pane under the second split pane. Its PTY shrinks,
    // while the first split pane keeps its rectangle and PTY size.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(first_split_pane_id)
            .unwrap()
            .len(),
        first_pane_resize_count_before
    );
    assert!(
        fake_pty_backend
            .list_pane_sizes(second_split_pane_id)
            .unwrap()
            .len()
            > second_pane_resize_count_before
    );
}

#[test]
fn new_pane_sibling_resize_failure_does_not_abort_the_command() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The first split spawns a sibling with a live PTY.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let first_split_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != root_pane_id)
        .expect("first split pane");
    let first_pane_resize_count_before = fake_pty_backend
        .list_pane_sizes(first_split_pane_id)
        .unwrap()
        .len();
    // The first split pane's next resize will error.
    fake_pty_backend.fail_resizes_on(
        first_split_pane_id,
        PtyError::UnknownPane {
            pane_id: first_split_pane_id,
        },
    );

    // The second split reflows the first split pane. Its resize errors, but the
    // command still succeeds and the new pane spawns; the failed resize records
    // nothing.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    let command_id = command_envelope.command_id;
    let command_result = runtime.dispatch(command_envelope);
    assert!(
        matches!(command_result, CommandResult::Ok { command_id: applied_command_id, .. } if applied_command_id == command_id)
    );
    let second_split_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != root_pane_id && *pane_id != first_split_pane_id)
        .expect("second split pane");
    assert!(runtime
        .pty_handle_by_pane_id
        .contains_key(&second_split_pane_id));
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(first_split_pane_id)
            .unwrap()
            .len(),
        first_pane_resize_count_before
    );
}

#[test]
fn new_pane_records_the_resolved_launch_cwd_on_the_command() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // `--cwd /work` with an explicit command whose own cwd is None: the command's
    // cwd resolves to /work at spawn.
    let mut spawn_spec = build_spawn_spec();
    spawn_spec.working_directory = None;
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            working_directory: Some(PathBuf::from("/work")),
            spawn_spec: Some(spawn_spec),
            ..build_new_pane_args()
        }),
    ));

    // The pane record's cwd, its command's own cwd, and the actual spawn cwd all agree
    // on the resolved launch directory — the pane record can't disagree with itself.
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    let pane_record = runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .unwrap();
    assert_eq!(pane_record.working_directory, Some(PathBuf::from("/work")));
    assert_eq!(
        pane_record.spawn_spec.as_ref().unwrap().working_directory,
        Some(PathBuf::from("/work"))
    );
    assert_eq!(
        fake_pty_backend
            .get_spawn_spec(new_pane_id)
            .unwrap()
            .working_directory,
        Some(PathBuf::from("/work"))
    );
}

#[test]
fn new_pane_records_an_explicit_commands_own_cwd() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The command carries its own cwd (/cmd); `--cwd /work` must NOT override it.
    let mut spawn_spec = build_spawn_spec();
    spawn_spec.working_directory = Some(PathBuf::from("/cmd"));
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            working_directory: Some(PathBuf::from("/work")),
            spawn_spec: Some(spawn_spec),
            ..build_new_pane_args()
        }),
    ));

    // Record and spawn both use the command's own /cmd — the pane record can't disagree
    // with what the process actually launched with.
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    let pane_record = runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .unwrap();
    assert_eq!(pane_record.working_directory, Some(PathBuf::from("/cmd")));
    assert_eq!(
        pane_record.spawn_spec.as_ref().unwrap().working_directory,
        Some(PathBuf::from("/cmd"))
    );
    assert_eq!(
        fake_pty_backend
            .get_spawn_spec(new_pane_id)
            .unwrap()
            .working_directory,
        Some(PathBuf::from("/cmd"))
    );
}

#[test]
fn new_pane_default_shell_records_the_cwd_and_no_command() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // No command, `--cwd /work`: the default shell launches in /work; the pane record
    // stores that cwd and no command (the request named none).
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            working_directory: Some(PathBuf::from("/work")),
            ..build_new_pane_args()
        }),
    ));

    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    let pane_record = runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .unwrap();
    assert_eq!(pane_record.working_directory, Some(PathBuf::from("/work")));
    assert_eq!(pane_record.spawn_spec, None);
    assert_eq!(
        fake_pty_backend
            .get_spawn_spec(new_pane_id)
            .unwrap()
            .working_directory,
        Some(PathBuf::from("/work"))
    );
}

#[test]
fn new_pane_external_into_a_viewed_tab_adopts_no_one() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // External CLI (no issuer, no --client) targeting a tab the client is already
    // viewing: the pane is created and sized to that viewer, but no one is
    // switched or focused — the client's focus stays on the root it was on.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(root_pane_id),
            ..build_new_pane_args()
        }),
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    let session = &runtime.session_by_id[&session_id];
    assert!(runtime.pty_handle_by_pane_id.contains_key(&new_pane_id));
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(root_pane_id)
    );
}

#[test]
fn new_pane_reflows_existing_sibling_ptys() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // First split creates pane A and spawns its PTY (the root has none yet).
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let pane_id_a = find_other_pane_id(&runtime, session_id, root_pane_id);
    let pane_resize_count_a_before = fake_pty_backend.list_pane_sizes(pane_id_a).unwrap().len();

    // Second split creates B off the now-focused A. A must reflow even though the
    // PTY-less root is in the layout — it must not abort the resize batch.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));

    // The second split reflows A exactly once more (spawn size + this reflow).
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id_a).unwrap().len(),
        pane_resize_count_a_before + 1
    );
    // The root, having no PTY, was never resized — confirming it was skipped.
    assert_eq!(
        fake_pty_backend.list_pane_sizes(root_pane_id),
        Err(PtyError::UnknownPane {
            pane_id: root_pane_id
        })
    );
}

#[test]
fn new_pane_explicit_command_inherits_the_pane_cwd() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // `--cwd /work` with an explicit command whose own cwd is None.
    let mut spawn_spec = build_spawn_spec();
    spawn_spec.working_directory = None;
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            working_directory: Some(PathBuf::from("/work")),
            spawn_spec: Some(spawn_spec),
            ..build_new_pane_args()
        }),
    ));

    // The command spawns in the pane cwd, not the inherited directory.
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    assert_eq!(
        fake_pty_backend
            .get_spawn_spec(new_pane_id)
            .unwrap()
            .working_directory,
        Some(PathBuf::from("/work"))
    );
}

#[test]
fn new_pane_explicit_command_cwd_wins_over_the_pane_cwd() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The command sets its own cwd, so the pane cwd does not override it.
    let mut spawn_spec = build_spawn_spec();
    spawn_spec.working_directory = Some(PathBuf::from("/cmd"));
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            working_directory: Some(PathBuf::from("/work")),
            spawn_spec: Some(spawn_spec),
            ..build_new_pane_args()
        }),
    ));

    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    assert_eq!(
        fake_pty_backend
            .get_spawn_spec(new_pane_id)
            .unwrap()
            .working_directory,
        Some(PathBuf::from("/cmd"))
    );
}

#[test]
fn new_pane_cross_session_sizes_to_a_target_session_viewer() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();

    // Session A holds the acting client.
    let client_id_a = ClientId::new();
    let session_id_a = SessionId::new();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session_a = build_bare_session(session_id_a);
    register_pane_record(&mut session_a, pane_id_a);
    register_session_tab(&mut session_a, tab_id_a, pane_id_a);
    attach_client(&mut session_a, client_id_a, tab_id_a, Some(pane_id_a));
    runtime.session_by_id.insert(session_id_a, session_a);

    // Session B owns the target pane and has its own client viewing B's tab with
    // a viewport distinct from the no-client default.
    let client_id_b = ClientId::new();
    let session_id_b = SessionId::new();
    let tab_id_b = TabId::new();
    let pane_id_b = PaneId::new();
    let mut session_b = build_bare_session(session_id_b);
    register_pane_record(&mut session_b, pane_id_b);
    register_session_tab(&mut session_b, tab_id_b, pane_id_b);
    let mut viewer = Client::from_attachment(
        client_id_b,
        session_id_b,
        SystemTime::now(),
        Size {
            column_count: 40,
            row_count: 10,
        },
        None,
        tab_id_b,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    viewer.update_focused_pane(tab_id_b, pane_id_b);
    session_b.attach_client(viewer);
    runtime.session_by_id.insert(session_id_b, session_b);

    // Cross-session --pane from A's client: no focus client in B, but B has a
    // viewer, so the new pane sizes to B's viewport, not the 80x24 default.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id_a),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(pane_id_b),
            ..build_new_pane_args()
        }),
    ));

    let new_pane_id = find_other_pane_id(&runtime, session_id_b, pane_id_b);
    assert!(runtime.pty_handle_by_pane_id.contains_key(&new_pane_id));
    // Exactly the size the production pipeline yields for a rightward split of
    // `pane_b` at B's 40x10 viewport — pins the target-viewer sizing and the
    // default split direction, not a loose bound (a vertical split would satisfy
    // any cols<=40/rows<=10 bound but produce a different exact size).
    let expected_pty_size = {
        let probe_pane_id = PaneId::new();
        let candidate_layout = split_leaf(
            &LayoutNode::Pane(pane_id_b),
            pane_id_b,
            probe_pane_id,
            Direction::Right,
        )
        .unwrap();
        let pane_content_rects = list_content_rects(&solve_layout_with_mode(
            &candidate_layout,
            LayoutMode::Tiled,
            Rect::from_size_at_origin(Size {
                column_count: 40,
                row_count: 8,
            }),
            PaneSizing::default(),
        ));
        let pane_rect = pane_content_rects
            .iter()
            .find(|(pane_id, _)| *pane_id == probe_pane_id)
            .and_then(|(_, pane_rect)| *pane_rect)
            .expect("new pane has a content rect");
        compute_pty_size(pane_rect)
    };
    assert_eq!(
        fake_pty_backend.list_pane_sizes(new_pane_id).unwrap()[0],
        expected_pty_size
    );
    assert_ne!(
        expected_pty_size,
        PtySize {
            column_count: 80,
            row_count: 24
        }
    );
}

#[test]
fn close_pane_defaults_to_the_focused_pane_and_kills_gracefully() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);

    // No explicit pane: the focused (split) pane closes. The root survives and
    // inherits focus — PaneClosing + PaneRemoved + LayoutChanged + PaneFocused.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .is_none());
    assert_eq!(
        runtime.session_by_id[&session_id].panes.pane_record_count(),
        1
    );
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(root_pane_id)
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(root_pane_id)
    );
    assert!(!runtime.pty_handle_by_pane_id.contains_key(&new_pane_id));
    assert!(!runtime.pty_size_by_pane_id.contains_key(&new_pane_id));
    // The default close policy is a graceful kill with the standard window.
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, new_pane_id),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION
        }]
    );
}

#[test]
fn close_pane_explicit_non_focused_target_keeps_focus() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);

    // Close the non-focused root explicitly: nobody's focus needs repair, so
    // PaneClosing + PaneRemoved + LayoutChanged + PtyResized(the surviving
    // split pane, now full-tab) are emitted and the client stays on the split
    // pane.
    let command_envelope = build_internal_command_envelope(Command::ClosePane(ClosePaneArgs {
        pane_id: Some(root_pane_id),
        should_force_close: false,
        should_kill_process_tree: false,
    }));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PtyResized"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(root_pane_id)
        .is_none());
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(new_pane_id)
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(new_pane_id)
    );
    assert!(runtime.pty_handle_by_pane_id.contains_key(&new_pane_id));
}

#[test]
fn close_pane_force_overrides_the_close_policy() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .panes
        .get_pane_record_mut_by_id(new_pane_id)
        .unwrap()
        .close_policy = PaneClosePolicy::ConfirmIfBusy;

    // `--force` wins over the pane's own policy: the close applies and the
    // child is force-killed, no busy question asked.
    let command_envelope = build_internal_command_envelope(Command::ClosePane(ClosePaneArgs {
        pane_id: Some(new_pane_id),
        should_force_close: true,
        should_kill_process_tree: false,
    }));
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .is_none());
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, new_pane_id),
        vec![KillPolicy::Force]
    );
}

#[test]
fn close_pane_tree_widens_the_graceful_kill_to_the_group() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);

    // The default-bound close key sends `tree: true`: the pane's graceful
    // policy keeps its window, widened to the whole process group so every
    // descendant stops with the shell.
    let command_envelope = build_internal_command_envelope(Command::ClosePane(ClosePaneArgs {
        pane_id: Some(new_pane_id),
        should_force_close: false,
        should_kill_process_tree: true,
    }));
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .is_none());
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, new_pane_id),
        vec![KillPolicy::GracefulTree {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION
        }]
    );
}

#[test]
fn close_pane_tree_with_force_group_kills_immediately() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);

    let command_envelope = build_internal_command_envelope(Command::ClosePane(ClosePaneArgs {
        pane_id: Some(new_pane_id),
        should_force_close: true,
        should_kill_process_tree: true,
    }));
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .is_none());
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, new_pane_id),
        vec![KillPolicy::Tree]
    );
}

#[test]
fn close_pane_confirm_if_busy_running_rejects_and_mutates_nothing() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .panes
        .get_pane_record_mut_by_id(new_pane_id)
        .unwrap()
        .close_policy = PaneClosePolicy::ConfirmIfBusy;

    // The pane's child is `Running`, so busy cannot be ruled out: the close
    // rejects and neither state nor process is touched.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane may be busy; pass --force to close anyway".to_string()),
        }
    );

    assert_eq!(
        runtime.session_by_id[&session_id].panes.pane_record_count(),
        2
    );
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id]
            .get_layout_tree()
            .list_leaf_pane_ids(),
        vec![root_pane_id, new_pane_id]
    );
    assert!(runtime.pty_handle_by_pane_id.contains_key(&new_pane_id));
    assert!(fake_pty_backend
        .list_pane_kill_policies(new_pane_id)
        .unwrap()
        .is_empty());
}

#[test]
fn close_pane_confirm_if_busy_exited_closes_gracefully() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    {
        let pane_record = runtime
            .session_by_id
            .get_mut(&session_id)
            .unwrap()
            .panes
            .get_pane_record_mut_by_id(new_pane_id)
            .unwrap();
        pane_record.close_policy = PaneClosePolicy::ConfirmIfBusy;
        pane_record
            .update_lifecycle(PaneLifecycleEvent::ProcessExited {
                exit_code: Some(0),
                exited_at: SystemTime::now(),
            })
            .unwrap();
    }

    // An `Exited` child is provably not busy: the close proceeds gracefully.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .is_none());
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, new_pane_id),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION
        }]
    );
}

#[test]
fn close_pane_confirm_if_busy_spawning_rejects() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .panes
        .get_pane_record_mut_by_id(pane_id)
        .unwrap()
        .close_policy = PaneClosePolicy::ConfirmIfBusy;

    // A `Spawning` pane's child has not started, so busy cannot be ruled out
    // either: same rejection as `Running`.
    let command_envelope = build_internal_command_envelope(Command::ClosePane(ClosePaneArgs {
        pane_id: Some(pane_id),
        should_force_close: false,
        should_kill_process_tree: false,
    }));
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane may be busy; pass --force to close anyway".to_string()),
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id].panes.pane_record_count(),
        1
    );
}

#[test]
fn close_pane_last_pane_closes_the_tab_and_quits() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client(&mut session, client_id, tab_id, Some(pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Closing the only pane empties the tab, which closes; that was the last
    // tab, so the session winds down — PaneClosing + PaneRemoved + TabClosed +
    // Quit.
    let command_envelope = build_internal_command_envelope(Command::ClosePane(ClosePaneArgs {
        pane_id: Some(pane_id),
        should_force_close: false,
        should_kill_process_tree: false,
    }));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "TabClosed", "Quit"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(runtime.session_by_id[&session_id].tabs.is_empty());
    assert_eq!(
        runtime.session_by_id[&session_id].panes.pane_record_count(),
        0
    );
    assert_eq!(
        *runtime.session_by_id[&session_id].get_lifecycle(),
        SessionLifecycle::Stopping
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        None
    );
}

#[test]
fn close_pane_last_pane_of_a_tab_moves_viewers_to_the_nearest_tab() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id, tab_id_b, Some(pane_id_b));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Emptying tab B closes it and moves its viewer to the surviving tab —
    // PaneClosing + PaneRemoved + TabClosed + TabFocused. The session keeps
    // running: another tab remains.
    let command_envelope = build_internal_command_envelope(Command::ClosePane(ClosePaneArgs {
        pane_id: Some(pane_id_b),
        should_force_close: false,
        should_kill_process_tree: false,
    }));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                [
                    "PaneClosing",
                    "PaneRemoved",
                    "TabClosed",
                    "TabFocused",
                    "PaneFocused"
                ]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(runtime.session_by_id[&session_id].tabs.len(), 1);
    assert!(runtime.session_by_id[&session_id]
        .tabs
        .contains_key(&tab_id_a));
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_a
    );
    // Unchanged from the setup: the session is not winding down.
    assert_eq!(
        *runtime.session_by_id[&session_id].get_lifecycle(),
        SessionLifecycle::Starting
    );
}

#[test]
fn close_pane_unviewed_tab_repairs_stored_focus() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let pane_id_c = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_pane_record(&mut session, pane_id_c);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    // Tab B holds a two-member stack with `pane_c` expanded; the client views
    // tab A but remembers `pane_c` as its focus in tab B.
    session
        .tabs
        .get_mut(&tab_id_b)
        .unwrap()
        .update_layout(LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![pane_id_b, pane_id_c],
            1,
        )));
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    // A second client with a smaller terminal, also viewing tab A: the
    // fallback viewport reduces across BOTH attached clients.
    let second_client_id = ClientId::new();
    let mut additional_client = Client::from_attachment(
        second_client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 60,
            row_count: 20,
        },
        None,
        tab_id_a,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    additional_client.update_focused_pane(tab_id_a, pane_id_a);
    session.attach_client(additional_client);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .clients
        .get_client_mut_by_id(client_id)
        .unwrap()
        .update_focused_pane(tab_id_b, pane_id_c);

    // Nobody views tab B, so its viewport falls back to the attached clients'
    // smallest; the stored focus entry still gets repaired onto the surviving
    // member — PaneClosing + PaneRemoved + LayoutChanged + PaneFocused.
    let command_envelope = build_internal_command_envelope(Command::ClosePane(ClosePaneArgs {
        pane_id: Some(pane_id_c),
        should_force_close: false,
        should_kill_process_tree: false,
    }));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id_b].get_layout_tree(),
        &LayoutNode::Pane(pane_id_b)
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id_b),
        Some(pane_id_b)
    );
    // The client's view never moved.
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_a
    );
}

#[test]
fn close_pane_reflows_surviving_pty_sizes() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Two splits: pane A (half width), then pane B splitting A (quarters).
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let pane_id_a = find_other_pane_id(&runtime, session_id, root_pane_id);
    let size_at_half = runtime.pty_size_by_pane_id[&pane_id_a];
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let pane_id_b = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != root_pane_id && *pane_id != pane_id_a)
        .expect("a third pane exists");
    assert_ne!(runtime.pty_size_by_pane_id[&pane_id_a], size_at_half);

    // Closing B collapses the split back to [root | A]: A reclaims exactly the
    // half-width geometry it had before B existed, and its PTY is resized to
    // it — PaneClosing + PaneRemoved + LayoutChanged + PaneFocused +
    // PtyResized(A).
    let resize_count_before_close = fake_pty_backend.list_pane_sizes(pane_id_a).unwrap().len();
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                [
                    "PaneClosing",
                    "PaneRemoved",
                    "LayoutChanged",
                    "PaneFocused",
                    "PtyResized"
                ]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id_a).unwrap().len(),
        resize_count_before_close + 1
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(pane_id_a)
            .unwrap()
            .last()
            .unwrap(),
        size_at_half
    );
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], size_at_half);
    assert!(runtime.pty_handle_by_pane_id.contains_key(&pane_id_a));
    assert!(!runtime.pty_size_by_pane_id.contains_key(&pane_id_b));
}

#[test]
fn close_pane_reflow_skips_a_survivor_whose_rect_is_unchanged() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // [root | A], then split root itself: [[root | B] | A]. A's right-half
    // rect is identical before and after B exists.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let pane_id_a = find_other_pane_id(&runtime, session_id, root_pane_id);
    let pane_pty_size_a = runtime.pty_size_by_pane_id[&pane_id_a];
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(root_pane_id),
            ..build_new_pane_args()
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], pane_pty_size_a);
    let pane_resize_count_a = fake_pty_backend.list_pane_sizes(pane_id_a).unwrap().len();

    // Closing B restores [root | A]. A's rect never changed, so the reflow
    // leaves its PTY alone: PaneClosing + PaneRemoved + LayoutChanged +
    // PaneFocused only — no PtyResized at all (root has no PTY).
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id_a).unwrap().len(),
        pane_resize_count_a
    );
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], pane_pty_size_a);
}

#[test]
fn close_pane_in_a_tab_with_no_viewer_keeps_pty_sizes() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let front_tab_id = TabId::new();
    let back_tab_id = TabId::new();
    let front_root_pane_id = PaneId::new();
    let back_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_root_pane_id);
    register_pane_record(&mut session, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_root_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);
    attach_client(
        &mut session,
        client_id,
        front_tab_id,
        Some(front_root_pane_id),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Split the front tab while viewed so it holds a live PTY, then adopt the
    // sole viewer onto the back tab: the front tab is left with no viewer.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let split_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != front_root_pane_id && *pane_id != back_pane_id)
        .expect("the front-tab split pane");
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(back_pane_id),
            ..build_new_pane_args()
        }),
    ));
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        back_tab_id
    );
    let resize_count_before = fake_pty_backend
        .list_pane_sizes(split_pane_id)
        .unwrap()
        .len();
    let split_pane_size_before = runtime.pty_size_by_pane_id[&split_pane_id];

    // Closing the unviewed front tab's other pane frees space, but a tab with
    // no viewer has no viewport: the surviving PTY keeps its last size.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(front_root_pane_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&front_tab_id].get_layout_tree(),
        &LayoutNode::Pane(split_pane_id)
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .len(),
        resize_count_before
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        split_pane_size_before
    );
}

#[test]
fn close_last_pane_reflows_the_tab_its_viewers_move_to() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let main_tab_id = TabId::new();
    let solo_tab_id = TabId::new();
    let main_root_pane_id = PaneId::new();
    let solo_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, main_root_pane_id);
    register_pane_record(&mut session, solo_pane_id);
    register_session_tab(&mut session, main_tab_id, main_root_pane_id);
    register_session_tab(&mut session, solo_tab_id, solo_pane_id);
    // One 40x10 client views the solo tab; another 80x24 client views the main
    // tab.
    let solo_viewer_client_id = ClientId::new();
    let mut solo_viewer = Client::from_attachment(
        solo_viewer_client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 40,
            row_count: 10,
        },
        None,
        solo_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    solo_viewer.update_focused_pane(solo_tab_id, solo_pane_id);
    session.attach_client(solo_viewer);
    let main_viewer_client_id = ClientId::new();
    attach_client(
        &mut session,
        main_viewer_client_id,
        main_tab_id,
        Some(main_root_pane_id),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Split the main tab so it holds a live PTY sized to the 80-wide viewport;
    // the solo-tab viewer is not a viewer of it yet.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(main_viewer_client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let main_split_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != main_root_pane_id && *pane_id != solo_pane_id)
        .expect("the main-tab split pane");
    let main_split_size_history_before = fake_pty_backend
        .list_pane_sizes(main_split_pane_id)
        .unwrap();

    // The solo-tab viewer closes its tab's only pane: the tab closes and that
    // viewer moves to the main tab, whose viewport reduces to 40x10. Its live PTY reflows
    // smaller. PaneClosing + PaneRemoved + TabClosed + TabFocused +
    // PtyResized(main_split_pane_id).
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(solo_viewer_client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                [
                    "PaneClosing",
                    "PaneRemoved",
                    "TabClosed",
                    "TabFocused",
                    "PaneFocused",
                    "PtyResized"
                ]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(solo_viewer_client_id)
            .unwrap()
            .get_active_tab(),
        main_tab_id
    );
    let main_split_size_history_after = fake_pty_backend
        .list_pane_sizes(main_split_pane_id)
        .unwrap();
    assert_eq!(
        main_split_size_history_after.len(),
        main_split_size_history_before.len() + 1
    );
    assert!(
        main_split_size_history_after.last().unwrap().column_count
            < main_split_size_history_before.last().unwrap().column_count
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&main_split_pane_id],
        *main_split_size_history_after.last().unwrap()
    );
}

#[test]
fn close_last_pane_of_an_unviewed_tab_reflows_nothing() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let front_tab_id = TabId::new();
    let back_tab_id = TabId::new();
    let front_root_pane_id = PaneId::new();
    let back_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_root_pane_id);
    register_pane_record(&mut session, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_root_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);
    attach_client(
        &mut session,
        client_id,
        front_tab_id,
        Some(front_root_pane_id),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // A live PTY on the viewed front tab.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let pane_id_x = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != front_root_pane_id && *pane_id != back_pane_id)
        .expect("the front-tab split pane");
    let pane_resize_count_x = fake_pty_backend.list_pane_sizes(pane_id_x).unwrap().len();
    let pane_pty_size_x = runtime.pty_size_by_pane_id[&pane_id_x];

    // Closing the unviewed back tab's only pane closes that tab; no viewer
    // moved, so no tab's viewport changed and nothing reflows — PaneClosing +
    // PaneRemoved + TabClosed only.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(back_pane_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "TabClosed"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert!(!runtime.session_by_id[&session_id]
        .tabs
        .contains_key(&back_tab_id));
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id_x).unwrap().len(),
        pane_resize_count_x
    );
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_x], pane_pty_size_x);
}

#[test]
fn close_pane_reflow_skips_a_collapsed_stack_member() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // [root | A], then stack B onto A: [root | stack(A collapsed, B expanded)].
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let pane_id_a = find_other_pane_id(&runtime, session_id, root_pane_id);
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            should_stack: true,
            ..build_new_pane_args()
        }),
    ));
    let pane_id_b = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != root_pane_id && *pane_id != pane_id_a)
        .expect("the stacked pane");
    let pane_resize_count_a = fake_pty_backend.list_pane_sizes(pane_id_a).unwrap().len();
    let pane_pty_size_a = runtime.pty_size_by_pane_id[&pane_id_a];
    let pane_resize_count_b = fake_pty_backend.list_pane_sizes(pane_id_b).unwrap().len();

    // Closing root hands the stack the full tab. The expanded member B
    // reflows wider; the collapsed member A has no content rect and keeps its
    // last size, with no event — PaneClosing + PaneRemoved + LayoutChanged +
    // PtyResized(B).
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(root_pane_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PtyResized"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let pane_size_history_b = fake_pty_backend.list_pane_sizes(pane_id_b).unwrap();
    assert_eq!(pane_size_history_b.len(), pane_resize_count_b + 1);
    assert_eq!(
        runtime.pty_size_by_pane_id[&pane_id_b],
        *pane_size_history_b.last().unwrap()
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id_a).unwrap().len(),
        pane_resize_count_a
    );
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], pane_pty_size_a);
}

#[test]
fn close_pane_repairs_focus_for_every_client_focused_on_it() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id_a = ClientId::new();
    let client_id_b = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id_a, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id_a),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);

    // A second client also focuses the split pane.
    let mut additional_client = Client::from_attachment(
        client_id_b,
        session_id,
        SystemTime::now(),
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    additional_client.update_focused_pane(tab_id, new_pane_id);
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .attach_client(additional_client);

    // Both clients focused the closed pane, so each gets its own repair —
    // PaneClosing + PaneRemoved + LayoutChanged + PaneFocused per client.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id_a),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                [
                    "PaneClosing",
                    "PaneRemoved",
                    "LayoutChanged",
                    "PaneFocused",
                    "PaneFocused"
                ]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    for client_id in [client_id_a, client_id_b] {
        assert_eq!(
            runtime.session_by_id[&session_id]
                .clients
                .get_client_by_id(client_id)
                .unwrap()
                .get_focused_pane(tab_id),
            Some(root_pane_id)
        );
    }
}

#[test]
fn close_pane_clears_only_the_gone_panes_view_state() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Split so the tab survives closing one pane; the split takes focus.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let split_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);

    // Scroll both panes up, and highlight text in the one about to close.
    {
        let scrolled = runtime
            .session_by_id
            .get_mut(&session_id)
            .unwrap()
            .clients
            .get_client_mut_by_id(client_id)
            .unwrap();
        scrolled.set_scroll_offset(split_pane_id, 5);
        scrolled.set_scroll_offset(root_pane_id, 3);
        scrolled.set_selection(
            split_pane_id,
            Selection {
                selection_kind: SelectionKind::Character,
                anchor: GridPosition {
                    row_index: 0,
                    column_index: 0,
                },
                cursor: GridPosition {
                    row_index: 0,
                    column_index: 4,
                },
            },
        );
    }

    // Close the focused (split) pane.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    let client_after_close = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .unwrap();
    // The closed pane's offset and highlight are dropped — a highlight over a
    // pane that no longer exists would keep holding a view of nothing. The
    // survivor's offset is untouched.
    assert_eq!(client_after_close.get_scroll_offset(split_pane_id), 0);
    assert_eq!(client_after_close.get_selection(split_pane_id), None);
    assert!(!client_after_close.is_view_held(split_pane_id));
    assert_eq!(client_after_close.get_scroll_offset(root_pane_id), 3);
    assert!(client_after_close.is_view_held(root_pane_id));
}

#[test]
fn close_tab_clears_the_view_state_of_its_panes() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id, tab_id_b, Some(pane_id_b));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Scroll the doomed tab's pane up and the survivor's too, and highlight text
    // in the doomed one.
    {
        let client = runtime
            .session_by_id
            .get_mut(&session_id)
            .unwrap()
            .clients
            .get_client_mut_by_id(client_id)
            .unwrap();
        client.set_scroll_offset(pane_id_b, 5);
        client.set_scroll_offset(pane_id_a, 3);
        client.set_selection(
            pane_id_b,
            Selection {
                selection_kind: SelectionKind::Character,
                anchor: GridPosition {
                    row_index: 0,
                    column_index: 0,
                },
                cursor: GridPosition {
                    row_index: 0,
                    column_index: 4,
                },
            },
        );
    }

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(tab_id_b),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    let client = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .unwrap();
    // Every pane of the closed tab loses its offset and its highlight; the
    // surviving tab's pane keeps its own offset.
    assert_eq!(client.get_scroll_offset(pane_id_b), 0);
    assert_eq!(client.get_selection(pane_id_b), None);
    assert!(!client.is_view_held(pane_id_b));
    assert_eq!(client.get_scroll_offset(pane_id_a), 3);
    assert!(client.is_view_held(pane_id_a));
}

#[test]
fn close_pane_stacked_member_collapses_the_stack() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            should_stack: true,
            ..build_new_pane_args()
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);

    // Closing the expanded stack member collapses the two-member stack back to
    // a plain leaf, and focus repairs onto the survivor.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .is_none());
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(root_pane_id)
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(root_pane_id)
    );
}

#[test]
fn close_pane_with_no_attached_clients_succeeds() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let tab_id = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id, pane_id_a);
    session
        .tabs
        .get_mut(&tab_id)
        .unwrap()
        .update_layout(LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![pane_id_a, pane_id_b],
            1,
        )));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // No client is attached anywhere, so the tab solves against the nominal
    // 80x24 viewport; with nobody's focus to repair, only PaneClosing +
    // PaneRemoved + LayoutChanged are emitted.
    let command_envelope = build_internal_command_envelope(Command::ClosePane(ClosePaneArgs {
        pane_id: Some(pane_id_b),
        should_force_close: false,
        should_kill_process_tree: false,
    }));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(pane_id_b)
        .is_none());
    assert_eq!(
        runtime.session_by_id[&session_id].panes.pane_record_count(),
        1
    );
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(pane_id_a)
    );
}

#[test]
fn close_pane_honors_the_panes_own_force_policy() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .panes
        .get_pane_record_mut_by_id(new_pane_id)
        .unwrap()
        .close_policy = PaneClosePolicy::Force;

    // Without `--force`, the pane's own configured policy decides: a `Force`
    // pane record force-kills the child.
    let command_envelope = build_internal_command_envelope(Command::ClosePane(ClosePaneArgs {
        pane_id: Some(new_pane_id),
        should_force_close: false,
        should_kill_process_tree: false,
    }));
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .is_none());
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, new_pane_id),
        vec![KillPolicy::Force]
    );
}

#[test]
fn close_pane_explicit_target_in_another_session_closes_there() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id_a = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b1 = PaneId::new();
    let pane_id_b2 = PaneId::new();

    let mut session_a = build_bare_session(SessionId::new());
    register_pane_record(&mut session_a, pane_id_a);
    register_session_tab(&mut session_a, tab_id_a, pane_id_a);
    attach_client(&mut session_a, client_id_a, tab_id_a, Some(pane_id_a));
    let session_id_a = session_a.session_id;
    runtime.session_by_id.insert(session_id_a, session_a);

    let mut session_b = build_bare_session(SessionId::new());
    register_pane_record(&mut session_b, pane_id_b1);
    register_pane_record(&mut session_b, pane_id_b2);
    register_session_tab(&mut session_b, tab_id_b, pane_id_b1);
    session_b
        .tabs
        .get_mut(&tab_id_b)
        .unwrap()
        .update_layout(LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![pane_id_b1, pane_id_b2],
            1,
        )));
    let session_id_b = session_b.session_id;
    runtime.session_by_id.insert(session_id_b, session_b);

    // An explicit pane target is global: issued by session A's client, it
    // closes the pane in its owning session B and leaves A untouched.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id_a),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(pane_id_b2),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert!(runtime.session_by_id[&session_id_b]
        .panes
        .get_pane_record_by_id(pane_id_b2)
        .is_none());
    assert_eq!(
        runtime.session_by_id[&session_id_b].tabs[&tab_id_b].get_layout_tree(),
        &LayoutNode::Pane(pane_id_b1)
    );
    assert_eq!(
        runtime.session_by_id[&session_id_a]
            .panes
            .pane_record_count(),
        1
    );
    assert_eq!(
        runtime.session_by_id[&session_id_a].tabs[&tab_id_a].get_layout_tree(),
        &LayoutNode::Pane(pane_id_a)
    );
    assert_eq!(
        runtime.session_by_id[&session_id_a]
            .clients
            .get_client_by_id(client_id_a)
            .unwrap()
            .get_focused_pane(tab_id_a),
        Some(pane_id_a)
    );
}

/// A horizontal two-pane split with equal weights, for building a tab's
/// layout directly.
fn build_horizontal_split(left_pane_id: PaneId, right_pane_id: PaneId) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(left_pane_id),
            LayoutNode::Pane(right_pane_id),
        ],
    ))
}

/// A session with one viewed tab whose pane was split once through dispatch,
/// so the new pane has a live PTY. Returns the runtime, fake backend, inbox
/// sender, ids, and the new pane's spawn-time PTY size.
fn build_resize_fixture() -> (
    Server,
    Arc<FakePtyBackend>,
    mpsc::Sender<RuntimeEvent>,
    SessionId,
    ClientId,
    PaneId,
    PaneId,
    PtySize,
) {
    let (mut runtime, fake_pty_backend, runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let pane_id_a = find_other_pane_id(&runtime, session_id, root_pane_id);
    let pane_pty_size_a = runtime.pty_size_by_pane_id[&pane_id_a];
    (
        runtime,
        fake_pty_backend,
        runtime_event_sender,
        session_id,
        client_id,
        root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    )
}

#[test]
fn resize_pane_grows_the_focused_pane_and_reflows_its_pty() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();

    // The client focuses A (the fresh split). Growing A's left border by 5
    // takes 5 columns from root; root has no PTY, so exactly one PtyResized
    // accompanies the LayoutChanged.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Left,
            resize_amount_cells: 5,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PtyResized"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let expected_pty_size = PtySize {
        column_count: pane_pty_size_a.column_count + 5,
        row_count: pane_pty_size_a.row_count,
    };
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], expected_pty_size);
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(pane_id_a)
            .unwrap()
            .last()
            .unwrap(),
        expected_pty_size
    );
}

#[test]
fn resize_pane_negative_size_shrinks_the_focused_pane() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();

    // The client focuses A. A negative size moves A's left border inward:
    // A gives 5 columns to root across that border.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Left,
            resize_amount_cells: -5,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PtyResized"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let expected_pty_size = PtySize {
        column_count: pane_pty_size_a.column_count - 5,
        row_count: pane_pty_size_a.row_count,
    };
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], expected_pty_size);
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(pane_id_a)
            .unwrap()
            .last()
            .unwrap(),
        expected_pty_size
    );
}

#[test]
fn resize_pane_via_in_session_cli_defaults_to_the_issuing_pane() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();

    // Issued from inside root's pane with no explicit target: root grows
    // right by 3, so its neighbor A donates 3 columns.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(client_id),
        root_pane_id,
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(
        command_source,
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Right,
            resize_amount_cells: 3,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    let expected_pty_size = PtySize {
        column_count: pane_pty_size_a.column_count - 3,
        row_count: pane_pty_size_a.row_count,
    };
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], expected_pty_size);
}

#[test]
fn resize_pane_explicit_target_resolves_its_owning_session() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        _client_id,
        _root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();

    // An internal source carries no session or client context; the explicit
    // pane target alone finds the owning session.
    let command_envelope = build_internal_command_envelope(Command::ResizePane(ResizePaneArgs {
        pane_id: Some(pane_id_a),
        direction: Direction::Left,
        resize_amount_cells: 2,
    }));
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    let expected_pty_size = PtySize {
        column_count: pane_pty_size_a.column_count + 2,
        row_count: pane_pty_size_a.row_count,
    };
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], expected_pty_size);
}

#[test]
fn resize_pane_min_size_rejection_reports_the_spare_and_mutates_nothing() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();
    let viewport = Size {
        column_count: 80,
        row_count: 24,
    };
    let rects_before = Server::tab_content_rects(
        &runtime.session_by_id[&session_id],
        runtime.session_by_id[&session_id]
            .tabs
            .keys()
            .copied()
            .next()
            .unwrap(),
        viewport,
        PaneSizing::default(),
    );
    let resize_count_before_rejection = fake_pty_backend.list_pane_sizes(pane_id_a).unwrap().len();

    // At 80 columns the donor root holds 40; its border-inclusive floor is 4
    // (the 2-column content minimum plus the 1-cell border on each side), so
    // it can give exactly 36. Asking for 100 rejects and leaves everything
    // untouched.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Left,
            resize_amount_cells: 100,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("the donating pane has only 36 spare cells to give".to_string()),
        }
    );

    let rects_after = Server::tab_content_rects(
        &runtime.session_by_id[&session_id],
        runtime.session_by_id[&session_id]
            .tabs
            .keys()
            .copied()
            .next()
            .unwrap(),
        viewport,
        PaneSizing::default(),
    );
    assert_eq!(rects_after, rects_before);
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id_a).unwrap().len(),
        resize_count_before_rejection
    );
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], pane_pty_size_a);
}

#[test]
fn dispatch_reporting_spare_hands_back_the_donors_spare_cells() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        _pane_id_a,
        _size_a,
    ) = build_resize_fixture();

    // The donor root holds 40 of the 80 columns and can give 36 before its own
    // floor. Asking for 100 refuses and hands that 36 back beside the result.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Left,
            resize_amount_cells: 100,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch_reporting_spare(command_envelope),
        (
            CommandResult::Rejected {
                command_id,
                reason: RejectReason::MinSize,
                help: Some("the donating pane has only 36 spare cells to give".to_string()),
            },
            Some(36)
        )
    );
}

#[test]
fn dispatch_reporting_spare_reports_no_spare_for_an_applied_resize() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        _pane_id_a,
        _size_a,
    ) = build_resize_fixture();

    // A resize the layout grants carries no spare: the second half is `None`
    // for every outcome but a border refused at a pane minimum.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Left,
            resize_amount_cells: 5,
        }),
    );
    let command_id = command_envelope.command_id;
    let (command_result, spare) = runtime.dispatch_reporting_spare(command_envelope);
    match command_result {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PtyResized"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(spare, None);
}

#[test]
fn a_border_refused_at_the_pane_minimum_writes_no_log_line() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        _pane_id_a,
        _size_a,
    ) = build_resize_fixture();
    let (_guard, logs) = koshi_observability::logging::with_test_writer();

    // A resize refused at a pane minimum is the one rejection that is not
    // logged, so the capture stays empty.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Left,
            resize_amount_cells: 100,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("the donating pane has only 36 spare cells to give".to_string()),
        }
    );
    assert_eq!(logs.contents(), "");
}

#[test]
fn resize_pane_at_the_tab_edge_moves_the_opposite_border_instead() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();

    // A touches the tab's right edge: no right border exists, so the left
    // border moves right instead — A shrinks by the cell and the left
    // sibling gains it.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Right,
            resize_amount_cells: 1,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let expected_pty_size = PtySize {
        column_count: pane_pty_size_a.column_count - 1,
        row_count: pane_pty_size_a.row_count,
    };
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], expected_pty_size);
}

#[test]
fn resize_pane_negative_size_at_the_edge_grows_via_the_opposite_border() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();

    // A negative size toward the tab edge (shrink away from a border that
    // does not exist) falls back the same way: the opposite border moves in
    // the same visual direction, so A grows by the cell.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Right,
            resize_amount_cells: -1,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let expected_pty_size = PtySize {
        column_count: pane_pty_size_a.column_count + 1,
        row_count: pane_pty_size_a.row_count,
    };
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], expected_pty_size);
}

#[test]
fn resize_pane_with_no_border_on_the_axis_is_rejected() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();

    // The layout has no vertical split level at all: Up finds no border and
    // neither does the opposite-side fallback, so the resize rejects whole.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Up,
            resize_amount_cells: 1,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane has no border to move on that axis".to_string()),
        }
    );
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], pane_pty_size_a);
}

#[test]
fn resize_pane_size_zero_is_rejected() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Left,
            resize_amount_cells: 0,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("resize size must be non-zero".to_string()),
        }
    );
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], pane_pty_size_a);
}

#[test]
fn resize_pane_with_no_attached_client_is_rejected() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let tab_id = TabId::new();
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, left_pane_id);
    register_pane_record(&mut session, right_pane_id);
    register_session_tab(&mut session, tab_id, left_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .unwrap()
        .update_layout(build_horizontal_split(left_pane_id, right_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    let viewport = Size {
        column_count: 80,
        row_count: 24,
    };
    let rects_before = Server::tab_content_rects(
        &runtime.session_by_id[&session_id],
        tab_id,
        viewport,
        PaneSizing::default(),
    );

    // No client is attached anywhere, so no tab is viewed and no terminal
    // displays the result.
    let command_envelope = build_internal_command_envelope(Command::ResizePane(ResizePaneArgs {
        pane_id: Some(left_pane_id),
        direction: Direction::Right,
        resize_amount_cells: 1,
    }));
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane's tab is not viewed by any client".to_string()),
        }
    );
    assert_eq!(
        Server::tab_content_rects(
            &runtime.session_by_id[&session_id],
            tab_id,
            viewport,
            PaneSizing::default()
        ),
        rects_before
    );
}

#[test]
fn resize_pane_in_an_unviewed_tab_is_rejected() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let front_tab_id = TabId::new();
    let back_tab_id = TabId::new();
    let front_root_pane_id = PaneId::new();
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_root_pane_id);
    register_pane_record(&mut session, left_pane_id);
    register_pane_record(&mut session, right_pane_id);
    register_session_tab(&mut session, front_tab_id, front_root_pane_id);
    register_session_tab(&mut session, back_tab_id, left_pane_id);
    session
        .tabs
        .get_mut(&back_tab_id)
        .unwrap()
        .update_layout(build_horizontal_split(left_pane_id, right_pane_id));
    attach_client(
        &mut session,
        client_id,
        front_tab_id,
        Some(front_root_pane_id),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    let viewport = Size {
        column_count: 80,
        row_count: 24,
    };
    let rects_before = Server::tab_content_rects(
        &runtime.session_by_id[&session_id],
        back_tab_id,
        viewport,
        PaneSizing::default(),
    );

    // A client is attached, but none views the back tab — no terminal
    // displays the result, so the resize rejects and mutates nothing.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: Some(left_pane_id),
            direction: Direction::Right,
            resize_amount_cells: 4,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane's tab is not viewed by any client".to_string()),
        }
    );
    assert_eq!(
        Server::tab_content_rects(
            &runtime.session_by_id[&session_id],
            back_tab_id,
            viewport,
            PaneSizing::default()
        ),
        rects_before
    );
}

#[test]
fn resize_pane_in_a_nested_split_moves_the_enclosing_border() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();

    // Split root again: [[root | B] | A]. B touches the inner split's right
    // edge, so growing B rightward moves the OUTER border — the whole inner
    // split takes 4 columns from A, and B's own share grows by 2.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(root_pane_id),
            ..build_new_pane_args()
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let pane_id_b = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != root_pane_id && *pane_id != pane_id_a)
        .expect("a third pane exists");
    let pane_pty_size_a = runtime.pty_size_by_pane_id[&pane_id_a];
    let pane_pty_size_b = runtime.pty_size_by_pane_id[&pane_id_b];

    // The client's focus followed the fresh split to B.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Right,
            resize_amount_cells: 4,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // LayoutChanged + PtyResized for A and B (root has no PTY).
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PtyResized", "PtyResized"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(
        runtime.pty_size_by_pane_id[&pane_id_a],
        PtySize {
            column_count: pane_pty_size_a.column_count - 4,
            row_count: pane_pty_size_a.row_count,
        }
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&pane_id_b],
        PtySize {
            column_count: pane_pty_size_b.column_count + 2,
            row_count: pane_pty_size_b.row_count,
        }
    );
}

// --- NewTab handler ----------------------------------------------------------

#[test]
fn new_tab_spawns_creates_and_focuses_for_the_issuer() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // TabCreated, PaneCreated, TabFocused, PaneFocused, PtyResized;
            // the vacated tab has no viewer left, so nothing else reflows.
            assert_eq!(
                list_event_names(&emitted_events),
                [
                    "TabCreated",
                    "PaneCreated",
                    "TabFocused",
                    "PaneFocused",
                    "PtyResized"
                ]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs.len(), 2);
    let new_tab = session
        .tabs
        .values()
        .find(|tab_id| tab_id.get_tab_id() != tab_id_a)
        .expect("the created tab");
    assert!(
        new_tab.get_tab_name().starts_with("T-"),
        "generated tab name, got {}",
        new_tab.get_tab_name()
    );
    assert_eq!(new_tab.get_tab_index(), 1);
    let new_pane_id = new_tab.get_layout_tree().list_leaf_pane_ids()[0];

    // The issuer switched onto the new tab and focuses its root pane.
    let client = session.clients.get_client_by_id(client_id).unwrap();
    assert_eq!(client.get_active_tab(), new_tab.get_tab_id());
    assert_eq!(
        client.get_focused_pane(new_tab.get_tab_id()),
        Some(new_pane_id)
    );

    // Root pane runs default shell in 80x22 middle region -> 78x20 content.
    let pane_record = session.panes.get_pane_record_by_id(new_pane_id).unwrap();
    assert_eq!(*pane_record.get_lifecycle(), PaneLifecycle::Running);
    assert_eq!(pane_record.spawn_spec, None);
    assert!(runtime.pty_handle_by_pane_id.contains_key(&new_pane_id));
    assert_eq!(
        fake_pty_backend.list_pane_sizes(new_pane_id).unwrap(),
        vec![PtySize {
            column_count: 78,
            row_count: 20
        }]
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&new_pane_id],
        PtySize {
            column_count: 78,
            row_count: 20
        }
    );
}

#[test]
fn new_tab_root_pane_carries_the_in_session_identity_env() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    ));

    // The root pane's spec is the default shell plus the identity vars naming
    // this session, the issuing client, and the root pane itself.
    let session = &runtime.session_by_id[&session_id];
    let new_tab = session
        .tabs
        .values()
        .find(|tab_id| tab_id.get_tab_id() != tab_id_a)
        .expect("the created tab");
    let new_pane_id = new_tab.get_layout_tree().list_leaf_pane_ids()[0];
    let mut expected_spawn_spec = runtime.build_default_shell_spec(None, BTreeMap::new());
    expected_spawn_spec
        .environment_variables
        .extend(build_koshi_environment(
            session_id,
            Some(client_id),
            new_pane_id,
            koshi_paths::resolve_runtime_directory().as_deref(),
        ));
    assert_eq!(
        fake_pty_backend.get_spawn_spec(new_pane_id).unwrap(),
        expected_spawn_spec
    );
}

#[test]
fn new_tab_generates_a_free_name() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    ));
    assert!(matches!(command_result, CommandResult::Ok { .. }));

    // The generated name is `T-<adjective>-<noun>` with both words drawn from
    // the same language's lists, and does not collide with the existing tab.
    let session = &runtime.session_by_id[&session_id];
    let new_tab = session
        .tabs
        .values()
        .find(|tab_id| tab_id.get_tab_id() != tab_id_a)
        .expect("the created tab");
    let mut pieces = new_tab.get_tab_name().splitn(3, '-');
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
    assert_ne!(new_tab.get_tab_name(), "t");
}

#[test]
fn new_tab_spawn_failure_commits_nothing() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    fake_pty_backend.fail_spawns_with(PtyError::Spawn {
        detail: "boom".to_string(),
    });
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("failed to launch the pane's process".to_string()),
        }
    );

    // Nothing was committed: no tab, no pane pane record, no view moved, no handle.
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs.len(), 1);
    assert_eq!(session.panes.pane_record_count(), 1);
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_a
    );
    assert!(runtime.pty_handle_by_pane_id.is_empty());
}

#[test]
fn new_tab_explicit_client_wins_over_the_issuer() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let issuer_client_id = ClientId::new();
    let named_client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    attach_client(&mut session, issuer_client_id, tab_id_a, Some(pane_id_a));
    attach_client(&mut session, named_client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(issuer_client_id),
        Command::NewTab(NewTabArgs {
            client_id: Some(named_client_id),
            ..NewTabArgs::default()
        }),
    ));
    assert!(matches!(command_result, CommandResult::Ok { .. }));

    let session = &runtime.session_by_id[&session_id];
    let new_tab_id = session
        .tabs
        .values()
        .find(|tab_id| tab_id.get_tab_id() != tab_id_a)
        .expect("the created tab")
        .get_tab_id();
    assert_eq!(
        session
            .clients
            .get_client_by_id(named_client_id)
            .unwrap()
            .get_active_tab(),
        new_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(issuer_client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_a
    );
}

#[test]
fn new_tab_with_an_unattached_explicit_client_is_rejected() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs {
            client_id: Some(ClientId::new()),
            ..NewTabArgs::default()
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    assert_eq!(runtime.session_by_id[&session_id].tabs.len(), 1);
    assert!(fake_pty_backend.list_spawned_pane_ids().is_empty());
}

#[test]
fn new_tab_external_source_defaults_to_the_sole_client() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        Command::NewTab(NewTabArgs::default()),
    ));
    assert!(matches!(command_result, CommandResult::Ok { .. }));

    let session = &runtime.session_by_id[&session_id];
    let new_tab_id = session
        .tabs
        .values()
        .find(|tab_id| tab_id.get_tab_id() != tab_id_a)
        .expect("the created tab")
        .get_tab_id();
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        new_tab_id
    );
}

#[test]
fn new_tab_external_source_with_two_clients_is_ambiguous() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    attach_client(&mut session, ClientId::new(), tab_id_a, None);
    attach_client(&mut session, ClientId::new(), tab_id_a, None);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        Command::NewTab(NewTabArgs::default()),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some("several clients are attached; name the target client".to_string()),
        }
    );
    assert!(fake_pty_backend.list_spawned_pane_ids().is_empty());
}

#[test]
fn new_tab_with_no_attached_client_is_stale() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        Command::NewTab(NewTabArgs::default()),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: Some("no client is attached to the session".to_string()),
        }
    );
    assert!(fake_pty_backend.list_spawned_pane_ids().is_empty());
}

#[test]
fn new_tab_reflows_the_vacated_tab_for_its_remaining_viewer() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let existing_tab_id = TabId::new();
    let existing_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, existing_pane_id);
    register_session_tab(&mut session, existing_tab_id, existing_pane_id);

    // The moving client is the 40x10 size constraint on the shared tab; the
    // remaining client views it at the full 80x24.
    let moving_client_id = ClientId::new();
    let mut moving_client = Client::from_attachment(
        moving_client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 40,
            row_count: 10,
        },
        None,
        existing_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    moving_client.update_focused_pane(existing_tab_id, existing_pane_id);
    session.attach_client(moving_client);
    let remaining_client_id = ClientId::new();
    attach_client(
        &mut session,
        remaining_client_id,
        existing_tab_id,
        Some(existing_pane_id),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Split the shared tab so it holds a live PTY sized to the 40x10
    // constraint: chrome leaves 40x8; half-columns yield 18x6 content.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(remaining_client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let split_pane_id = find_other_pane_id(&runtime, session_id, existing_pane_id);
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        PtySize {
            column_count: 18,
            row_count: 6
        }
    );

    // The moving client creates a new tab and leaves: the vacated pane region
    // grows to 80x22, giving 38x20 content per half. Its new 40x8 pane region
    // gives 38x6.
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(moving_client_id),
        Command::NewTab(NewTabArgs::default()),
    ));
    match command_result {
        CommandResult::Ok { emitted_events, .. } => {
            // TabCreated, PaneCreated, TabFocused, PaneFocused, PtyResized
            // (spawn), then the vacated tab's one live PTY reflowed
            // The original pane never spawned, so only the split pane resizes.
            assert_eq!(
                list_event_names(&emitted_events),
                [
                    "TabCreated",
                    "PaneCreated",
                    "TabFocused",
                    "PaneFocused",
                    "PtyResized",
                    "PtyResized"
                ]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        PtySize {
            column_count: 38,
            row_count: 20
        }
    );
    let new_tab = runtime.session_by_id[&session_id]
        .tabs
        .values()
        .find(|tab_id| tab_id.get_tab_id() != existing_tab_id)
        .expect("the created tab");
    let created_pane_id = new_tab.get_layout_tree().list_leaf_pane_ids()[0];
    assert_eq!(
        fake_pty_backend.list_pane_sizes(created_pane_id).unwrap(),
        vec![PtySize {
            column_count: 38,
            row_count: 6
        }]
    );
}

// --- CloseTab handler ----------------------------------------------------------

#[test]
fn close_tab_removes_state_kills_children_and_moves_viewers() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id, tab_id_b, Some(pane_id_b));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Give the doomed tab a live PTY by splitting it while viewed.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let pane_id_x = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != pane_id_a && *pane_id != pane_id_b)
        .expect("the split pane");

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(tab_id_b),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // PaneClosing+PaneRemoved for each of the two panes, TabClosed,
            // TabFocused (the viewer moves to tab_a; its pane has no PTY, so
            // nothing reflows).
            assert_eq!(
                list_event_names(&emitted_events),
                [
                    "PaneClosing",
                    "PaneRemoved",
                    "PaneClosing",
                    "PaneRemoved",
                    "TabClosed",
                    "TabFocused",
                    "PaneFocused"
                ]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let session = &runtime.session_by_id[&session_id];
    assert!(!session.tabs.contains_key(&tab_id_b));
    assert!(session.panes.get_pane_record_by_id(pane_id_b).is_none());
    assert!(session.panes.get_pane_record_by_id(pane_id_x).is_none());
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_a
    );
    assert!(!runtime.pty_handle_by_pane_id.contains_key(&pane_id_x));
    assert!(!runtime.pty_size_by_pane_id.contains_key(&pane_id_x));
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, pane_id_x),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION
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
    fn spawn_pane(
        &self,
        pane_id: PaneId,
        spawn_spec: SpawnSpec,
        pty_size: PtySize,
    ) -> Result<PtyHandle, PtyError> {
        self.inner.spawn_pane(pane_id, spawn_spec, pty_size)
    }
    fn resize_pane(&self, pane_id: PaneId, pty_size: PtySize) -> Result<(), PtyError> {
        self.inner.resize_pane(pane_id, pty_size)
    }
    fn write_pane_input(&self, pane_id: PaneId, input_bytes: &[u8]) -> Result<(), PtyError> {
        self.inner.write_pane_input(pane_id, input_bytes)
    }
    fn kill_pane(&self, pane_id: PaneId, kill_policy: KillPolicy) -> Result<(), PtyError> {
        self.barrier.wait();
        self.inner.kill_pane(pane_id, kill_policy)
    }
    fn find_live_working_directory(&self, pane_id: PaneId) -> Option<PathBuf> {
        self.inner.find_live_working_directory(pane_id)
    }
}

#[test]
fn close_tab_kills_every_pane_concurrently() {
    // The doomed tab holds three panes (the PTY-less root plus two spawned
    // splits); the barrier releases a kill only once all three have started.
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(BarrierKillBackend {
        inner: fake_pty_backend.clone(),
        barrier: Barrier::new(3),
    });
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let mut runtime = Server::from_runtime_parts(
        pty_backend,
        runtime_event_receiver,
        runtime_event_sender.clone(),
    );

    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id, tab_id_b, Some(pane_id_b));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Two splits give the doomed tab two live PTYs.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let spawned: Vec<PaneId> = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .filter(|pane_id| *pane_id != pane_id_a && *pane_id != pane_id_b)
        .collect();
    let [pane_id_x, pane_id_y] = spawned[..] else {
        panic!("expected exactly two split panes, got {spawned:?}");
    };

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(tab_id_b),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    // Both live children die with their own graceful policy; reaching this at
    // all proves the kills started concurrently, or the barrier would have
    // held the lone serial kill thread forever.
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, pane_id_x),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION
        }]
    );
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, pane_id_y),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION
        }]
    );
}

#[test]
fn close_tab_with_a_busy_confirm_pane_rejects_without_force() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id, tab_id_b, Some(pane_id_b));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let pane_id_x = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != pane_id_a && *pane_id != pane_id_b)
        .expect("the split pane");
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .panes
        .get_pane_record_mut_by_id(pane_id_x)
        .unwrap()
        .close_policy = PaneClosePolicy::ConfirmIfBusy;

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(tab_id_b),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("a pane in the tab may be busy; pass --force to close anyway".to_string()),
        }
    );

    // All-or-nothing: nothing was closed, nothing killed.
    let session = &runtime.session_by_id[&session_id];
    assert!(session.tabs.contains_key(&tab_id_b));
    assert!(session.panes.get_pane_record_by_id(pane_id_x).is_some());
    assert!(runtime.pty_handle_by_pane_id.contains_key(&pane_id_x));
    assert!(fake_pty_backend
        .list_pane_kill_policies(pane_id_x)
        .unwrap()
        .is_empty());
}

#[test]
fn close_tab_force_kills_a_busy_confirm_pane() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id, tab_id_b, Some(pane_id_b));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let pane_id_x = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != pane_id_a && *pane_id != pane_id_b)
        .expect("the split pane");
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .panes
        .get_pane_record_mut_by_id(pane_id_x)
        .unwrap()
        .close_policy = PaneClosePolicy::ConfirmIfBusy;

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(tab_id_b),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
    ));
    assert!(matches!(command_result, CommandResult::Ok { .. }));
    assert!(!runtime.session_by_id[&session_id]
        .tabs
        .contains_key(&tab_id_b));
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, pane_id_x),
        vec![KillPolicy::Force]
    );
}

#[test]
fn close_tab_confirm_if_busy_exited_pane_closes() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id, tab_id_b, Some(pane_id_b));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let pane_id_x = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != pane_id_a && *pane_id != pane_id_b)
        .expect("the split pane");
    {
        let pane_record = runtime
            .session_by_id
            .get_mut(&session_id)
            .unwrap()
            .panes
            .get_pane_record_mut_by_id(pane_id_x)
            .unwrap();
        pane_record.close_policy = PaneClosePolicy::ConfirmIfBusy;
        pane_record
            .update_lifecycle(PaneLifecycleEvent::ProcessExited {
                exit_code: Some(0),
                exited_at: SystemTime::now(),
            })
            .unwrap();
    }

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(tab_id_b),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    ));
    assert!(matches!(command_result, CommandResult::Ok { .. }));
    assert!(!runtime.session_by_id[&session_id]
        .tabs
        .contains_key(&tab_id_b));
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, pane_id_x),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION
        }]
    );
}

#[test]
fn close_last_tab_quits_the_session() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // PaneClosing, PaneRemoved, TabClosed, Quit.
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "TabClosed", "Quit"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert!(session.tabs.is_empty());
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn close_tab_with_an_unknown_explicit_tab_is_not_found() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(TabId::new()),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
    assert_eq!(runtime.session_by_id[&session_id].tabs.len(), 1);
}

#[test]
fn close_tab_reflows_the_tab_its_viewers_move_to() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let remaining_tab_id = TabId::new();
    let closing_tab_id = TabId::new();
    let remaining_root_pane_id = PaneId::new();
    let closing_root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, remaining_root_pane_id);
    register_pane_record(&mut session, closing_root_pane_id);
    register_session_tab(&mut session, remaining_tab_id, remaining_root_pane_id);
    register_session_tab(&mut session, closing_tab_id, closing_root_pane_id);

    // One client (80x24) views the remaining tab; the moving client (40x10)
    // views the closing tab.
    let remaining_client_id = ClientId::new();
    attach_client(
        &mut session,
        remaining_client_id,
        remaining_tab_id,
        Some(remaining_root_pane_id),
    );
    let moving_client_id = ClientId::new();
    let moving_client = Client::from_attachment(
        moving_client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 40,
            row_count: 10,
        },
        None,
        closing_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    session.attach_client(moving_client);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Split the remaining tab while only its 80x24 viewer sees it: it leaves an 80x22 pane region,
    // so each half's content is 38x20.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(remaining_client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let split_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != remaining_root_pane_id && *pane_id != closing_root_pane_id)
        .expect("the split pane");
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        PtySize {
            column_count: 38,
            row_count: 20
        }
    );

    // The moving client closes its tab and joins the remaining tab: 40x10 leaves a 40x8 pane region,
    // so each half's content is 18x6.
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(moving_client_id),
        Command::CloseTab(CloseTabArgs::default()),
    ));
    match command_result {
        CommandResult::Ok { emitted_events, .. } => {
            // PaneClosing, PaneRemoved, TabClosed, TabFocused, PtyResized.
            assert_eq!(
                list_event_names(&emitted_events),
                [
                    "PaneClosing",
                    "PaneRemoved",
                    "TabClosed",
                    "TabFocused",
                    "PaneFocused",
                    "PtyResized"
                ]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        PtySize {
            column_count: 18,
            row_count: 6
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(moving_client_id)
            .unwrap()
            .get_active_tab(),
        remaining_tab_id
    );
}

#[test]
fn move_tab_reorders_and_emits() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let tab_id_c = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let pane_id_c = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_pane_record(&mut session, pane_id_c);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    register_session_tab(&mut session, tab_id_c, pane_id_c);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Explicit tab C (slot 2) to the front.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::MoveTab(MoveTabArgs {
            tab_id: Some(tab_id_c),
            target_tab_index: 0,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(list_event_names(&emitted_events), ["TabMoved"]);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    // New order C, A, B — the others closed ranks behind the moved tab.
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs[&tab_id_c].get_tab_index(), 0);
    assert_eq!(session.tabs[&tab_id_a].get_tab_index(), 1);
    assert_eq!(session.tabs[&tab_id_b].get_tab_index(), 2);
    // Order-only change: the client still views the same tab.
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_a
    );
}

#[test]
fn move_tab_defaults_to_the_issuers_active_tab() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    // The issuer views tab B (slot 1).
    attach_client(&mut session, client_id, tab_id_b, Some(pane_id_b));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::MoveTab(MoveTabArgs {
            tab_id: None,
            target_tab_index: 0,
        }),
    ));
    match command_result {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(list_event_names(&emitted_events), ["TabMoved"])
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs[&tab_id_b].get_tab_index(), 0);
    assert_eq!(session.tabs[&tab_id_a].get_tab_index(), 1);
}

#[test]
fn move_tab_clamps_an_out_of_range_index() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let tab_id_c = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let pane_id_c = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_pane_record(&mut session, pane_id_c);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    register_session_tab(&mut session, tab_id_c, pane_id_c);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Index 99 clamps to the last slot (2).
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::MoveTab(MoveTabArgs {
            tab_id: Some(tab_id_a),
            target_tab_index: 99,
        }),
    ));
    match command_result {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(list_event_names(&emitted_events), ["TabMoved"])
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs[&tab_id_b].get_tab_index(), 0);
    assert_eq!(session.tabs[&tab_id_c].get_tab_index(), 1);
    assert_eq!(session.tabs[&tab_id_a].get_tab_index(), 2);
}

#[test]
fn move_tab_to_its_current_slot_is_ok_with_no_events() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::MoveTab(MoveTabArgs {
            tab_id: Some(tab_id_a),
            target_tab_index: 0,
        }),
    ));
    match command_result {
        CommandResult::Ok { emitted_events, .. } => assert!(emitted_events.is_empty()),
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs[&tab_id_a].get_tab_index(), 0);
    assert_eq!(session.tabs[&tab_id_b].get_tab_index(), 1);
}

#[test]
fn in_session_cli_move_tab_defaults_to_the_source_pane_tab() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(session_id);
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    // Client's active tab is B, but the CLI command was issued from tab A's pane.
    attach_client(&mut session, client_id, tab_id_b, None);
    runtime.session_by_id.insert(session.session_id, session);

    // MoveTab with no explicit tab — InSessionCli resolves via the tab
    // containing pane_a (tab A), not the client's active tab (tab B).
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(client_id),
        pane_id_a,
        PathBuf::from("/sock"),
    );
    let command_result = runtime.dispatch(build_command_envelope(
        command_source,
        Command::MoveTab(MoveTabArgs {
            tab_id: None,
            target_tab_index: 1,
        }),
    ));
    match command_result {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(list_event_names(&emitted_events), ["TabMoved"])
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs[&tab_id_b].get_tab_index(), 0);
    assert_eq!(session.tabs[&tab_id_a].get_tab_index(), 1);
}

#[test]
fn move_tab_with_an_unknown_tab_is_rejected() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::MoveTab(MoveTabArgs {
            tab_id: Some(TabId::new()),
            target_tab_index: 0,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
    // A rejected move mutates nothing.
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id_a].get_tab_index(),
        0
    );
}

// --- FocusTab handler ----------------------------------------------------------

#[test]
fn focus_tab_switches_the_view_and_emits() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(tab_id_b),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // The client held no focus in the tab it switches to, so it lands
            // on that tab's landing pane. Neither tab holds a live PTY to reflow.
            assert_eq!(
                list_event_names(&emitted_events),
                ["TabFocused", "PaneFocused"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_b
    );
}

#[test]
fn focus_tab_index_next_and_prev_resolve_against_the_display_order() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a); // index 0
    register_session_tab(&mut session, tab_id_b, pane_id_b); // index 1
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    let get_active_tab_id = |runtime: &Server| {
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab()
    };

    // Next from tab_a (index 0) steps to tab_b (index 1).
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Next,
            client_id: None,
        }),
    ));
    assert!(matches!(command_result, CommandResult::Ok { .. }));
    assert_eq!(get_active_tab_id(&runtime), tab_id_b);

    // Next from the last tab wraps to the first.
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Next,
            client_id: None,
        }),
    ));
    assert!(matches!(command_result, CommandResult::Ok { .. }));
    assert_eq!(get_active_tab_id(&runtime), tab_id_a);

    // Prev from the first tab wraps to the last.
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Prev,
            client_id: None,
        }),
    ));
    assert!(matches!(command_result, CommandResult::Ok { .. }));
    assert_eq!(get_active_tab_id(&runtime), tab_id_b);

    // An explicit index resolves the tab at that display position.
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Index(0),
            client_id: None,
        }),
    ));
    assert!(matches!(command_result, CommandResult::Ok { .. }));
    assert_eq!(get_active_tab_id(&runtime), tab_id_a);
}

#[test]
fn focus_tab_on_the_already_active_tab_is_ok_with_no_events() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(tab_id_a),
            client_id: None,
        }),
    ));
    match command_result {
        CommandResult::Ok { emitted_events, .. } => assert!(emitted_events.is_empty()),
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_a
    );
}

#[test]
fn focus_tab_with_an_unknown_id_or_index_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    for tab_target in [TabTarget::Id(TabId::new()), TabTarget::Index(9)] {
        let command_envelope = build_command_envelope(
            CommandSource::from_key_binding(client_id),
            Command::FocusTab(FocusTabArgs {
                focus_target: tab_target,
                client_id: None,
            }),
        );
        let command_id = command_envelope.command_id;
        assert_eq!(
            runtime.dispatch(command_envelope),
            CommandResult::Rejected {
                command_id,
                reason: RejectReason::TargetNotFound,
                help: None,
            }
        );
    }
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_a
    );
}

#[test]
fn focus_tab_explicit_client_wins_over_the_issuer() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let issuer_client_id = ClientId::new();
    let named_client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, issuer_client_id, tab_id_a, Some(pane_id_a));
    attach_client(&mut session, named_client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(issuer_client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(tab_id_b),
            client_id: Some(named_client_id),
        }),
    ));
    assert!(matches!(command_result, CommandResult::Ok { .. }));

    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(named_client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_b
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(issuer_client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_a
    );
}

#[test]
fn focus_tab_with_an_unattached_explicit_client_is_rejected() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(tab_id_b),
            client_id: Some(ClientId::new()),
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_a
    );
}

#[test]
fn focus_tab_external_source_defaults_to_the_sole_client() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(tab_id_b),
            client_id: None,
        }),
    ));
    assert!(matches!(command_result, CommandResult::Ok { .. }));
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_b
    );
}

#[test]
fn focus_tab_external_source_with_two_clients_is_ambiguous() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    attach_client(&mut session, ClientId::new(), tab_id_a, None);
    attach_client(&mut session, ClientId::new(), tab_id_a, None);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(tab_id_a),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some("several clients are attached; name the target client".to_string()),
        }
    );
}

#[test]
fn focus_tab_with_no_attached_client_is_stale() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(tab_id_a),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: Some("no client is attached to the session".to_string()),
        }
    );
}

#[test]
fn focus_tab_reflows_both_the_target_and_the_left_tab() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let moving_client_tab_id = TabId::new();
    let staying_client_tab_id = TabId::new();
    let moving_client_root_pane_id = PaneId::new();
    let staying_client_root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, moving_client_root_pane_id);
    register_pane_record(&mut session, staying_client_root_pane_id);
    register_session_tab(
        &mut session,
        moving_client_tab_id,
        moving_client_root_pane_id,
    );
    register_session_tab(
        &mut session,
        staying_client_tab_id,
        staying_client_root_pane_id,
    );

    // The moving client (30x8) starts on its tab; the staying client (40x10)
    // views the other tab.
    let moving_client_id = ClientId::new();
    let moving_client = Client::from_attachment(
        moving_client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 30,
            row_count: 8,
        },
        None,
        moving_client_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    session.attach_client(moving_client);
    let staying_client_id = ClientId::new();
    let mut staying_client = Client::from_attachment(
        staying_client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 40,
            row_count: 10,
        },
        None,
        staying_client_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    staying_client.update_focused_pane(staying_client_tab_id, staying_client_root_pane_id);
    session.attach_client(staying_client);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Split the staying client's tab while only its 40x10 viewer sees it: chrome leaves 40x8,
    // so the new half-column PTY content is 18x6.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(staying_client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let split_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| {
            *pane_id != moving_client_root_pane_id && *pane_id != staying_client_root_pane_id
        })
        .expect("the split pane");
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        PtySize {
            column_count: 18,
            row_count: 6
        }
    );

    // The moving client switches onto the staying client's tab: full viewport minimum is 30x8, leaving a 30x6
    // pane region; each half's content is 13x4.
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(moving_client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(staying_client_tab_id),
            client_id: None,
        }),
    ));
    match command_result {
        CommandResult::Ok { emitted_events, .. } => {
            // TabFocused + the tightened PTY's resize (tab_a holds no PTY).
            assert_eq!(
                list_event_names(&emitted_events),
                ["TabFocused", "PaneFocused", "PtyResized"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        PtySize {
            column_count: 13,
            row_count: 4
        }
    );

    // The moving client switches back to its tab: the staying client's tab loses
    // the 30x8 constraint and reflows to the 40x10 geometry.
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(moving_client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(moving_client_tab_id),
            client_id: None,
        }),
    ));
    assert!(matches!(command_result, CommandResult::Ok { .. }));
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        PtySize {
            column_count: 18,
            row_count: 6
        }
    );
}

#[test]
fn new_tab_for_a_client_below_minimum_size_is_rejected() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    // A 1x1 viewport cannot hold even one minimum-size pane.
    let client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 1,
            row_count: 1,
        },
        None,
        tab_id_a,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    session.attach_client(client);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("not enough space for a new tab".to_string()),
        }
    );
    assert_eq!(runtime.session_by_id[&session_id].tabs.len(), 1);
    assert!(fake_pty_backend.list_spawned_pane_ids().is_empty());
}

/// The [`build_resize_fixture`] tab zoomed onto the split pane through dispatch:
/// mode `Fullscreen { focused: pane_a }`, its PTY resized to the full-tab
/// content rect (80x24 viewport -> 78x22).
fn build_fullscreen_fixture() -> (
    Server,
    Arc<FakePtyBackend>,
    mpsc::Sender<RuntimeEvent>,
    SessionId,
    ClientId,
    PaneId,
    PaneId,
    PtySize,
) {
    let (
        mut runtime,
        fake_pty_backend,
        runtime_event_sender,
        session_id,
        client_id,
        root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::TogglePaneFullscreen,
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    (
        runtime,
        fake_pty_backend,
        runtime_event_sender,
        session_id,
        client_id,
        root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    )
}

/// The id of the session's single tab.
fn get_only_tab_id(runtime: &Server, session_id: SessionId) -> TabId {
    runtime.session_by_id[&session_id]
        .tabs
        .keys()
        .copied()
        .next()
        .unwrap()
}

/// How `client_id` sees `tab_id` laid out. Zoom is per-client, so the mode is read
/// off the client — the tab holds only the tree, and two clients on one tab can
/// answer this differently.
fn get_client_layout_mode(
    runtime: &Server,
    session_id: SessionId,
    client_id: ClientId,
    tab_id: TabId,
) -> LayoutMode {
    runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_layout_mode(tab_id)
}

#[test]
fn toggle_fullscreen_promotes_the_focused_pane_and_reflows_its_pty() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);
    let tree_before = runtime.session_by_id[&session_id].tabs[&tab_id]
        .get_layout_tree()
        .clone();

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::TogglePaneFullscreen,
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            // LayoutChanged plus the promoted pane's PtyResized; the hidden
            // root has no PTY and the focus was already on the pane.
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PtyResized"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: pane_id_a
        }
    );
    // The mode is a solve-time overlay: the tree itself is untouched.
    assert_eq!(*session.tabs[&tab_id].get_layout_tree(), tree_before);
    let fullscreen_pty_size = PtySize {
        column_count: 78,
        row_count: 20,
    };
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], fullscreen_pty_size);
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(pane_id_a)
            .unwrap()
            .last()
            .unwrap(),
        fullscreen_pty_size
    );
}

#[test]
fn toggle_fullscreen_off_restores_the_exact_prior_layout_and_sizes() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_fullscreen_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);
    let tree_before = runtime.session_by_id[&session_id].tabs[&tab_id]
        .get_layout_tree()
        .clone();

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::TogglePaneFullscreen,
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => {
            // LayoutChanged plus the pane shrinking back to its tiled rect.
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PtyResized"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Tiled
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(*session.tabs[&tab_id].get_layout_tree(), tree_before);
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], pane_pty_size_a);
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(pane_id_a)
            .unwrap()
            .last()
            .unwrap(),
        pane_pty_size_a
    );
}

#[test]
fn toggle_fullscreen_from_the_issuing_pane_moves_the_acting_focus() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);

    // Issued from inside root's pane while the client's focus is on the
    // split pane: root is promoted and the focus follows it.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(client_id),
        root_pane_id,
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(command_source, Command::TogglePaneFullscreen);
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => {
            // LayoutChanged plus PaneFocused: root has no PTY to resize, and
            // the hidden pane keeps its last size eventlessly.
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PaneFocused"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: root_pane_id
        }
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(root_pane_id)
    );
    assert_eq!(
        session.tabs[&tab_id].list_focus_mru().first(),
        Some(&root_pane_id)
    );
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], pane_pty_size_a);
}

#[test]
fn toggle_fullscreen_on_an_unviewed_tab_is_rejected() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Issued from inside the unviewed tab's pane: the toggle would resize
    // real PTYs against a viewport no client provides.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(client_id),
        pane_id_b,
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(command_source, Command::TogglePaneFullscreen);
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane's tab is not viewed by any client".to_string()),
        }
    );
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id_b),
        LayoutMode::Tiled
    );
}

#[test]
fn toggle_fullscreen_below_the_pane_floor_is_rejected() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    // A 3x3 viewport is below the border-inclusive floor (4 columns), so
    // even the whole tab cannot show the pane's content minimum.
    let mut client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::now(),
        Size {
            column_count: 3,
            row_count: 3,
        },
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, pane_id);
    session.attach_client(client);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::TogglePaneFullscreen,
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("not enough space to fullscreen the pane".to_string()),
        }
    );
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Tiled
    );
}

#[test]
fn focus_pane_under_fullscreen_retargets_the_zoom() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_fullscreen_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);
    let fullscreen_pty_size = PtySize {
        column_count: 78,
        row_count: 20,
    };

    // Root is hidden behind the fullscreen — focusing it swaps the zoom to
    // it instead of rejecting or dropping the mode.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(root_pane_id),
            client_id: None,
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => {
            // LayoutChanged plus PaneFocused: root has no PTY, and the
            // newly hidden pane keeps its last size eventlessly.
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PaneFocused"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: root_pane_id
        }
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(root_pane_id)
    );
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], fullscreen_pty_size);
}

#[test]
fn focus_pane_retargeting_back_skips_the_unchanged_pty() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_fullscreen_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);
    let fullscreen_pty_size = PtySize {
        column_count: 78,
        row_count: 20,
    };

    let build_focus_event = |pane_id: PaneId| {
        build_command_envelope(
            CommandSource::from_key_binding(client_id),
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Pane(pane_id),
                client_id: None,
            }),
        )
    };
    assert!(matches!(
        runtime.dispatch(build_focus_event(root_pane_id)),
        CommandResult::Ok { .. }
    ));
    let resize_count_before_retarget = fake_pty_backend.list_pane_sizes(pane_id_a).unwrap().len();

    // Retargeting back gives the pane the same full-tab rect it last held,
    // so the reflow applies nothing.
    match runtime.dispatch(build_focus_event(pane_id_a)) {
        CommandResult::Ok { emitted_events, .. } => {
            // LayoutChanged plus PaneFocused, no PtyResized.
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PaneFocused"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: pane_id_a
        }
    );
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], fullscreen_pty_size);
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id_a).unwrap().len(),
        resize_count_before_retarget
    );
}

#[test]
fn focus_pane_on_the_promoted_pane_is_a_no_op() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_fullscreen_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(pane_id_a),
            client_id: None,
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(emitted_events, Vec::new());
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: pane_id_a
        }
    );
}

/// One client zooming a pane changes nothing for another client on the same tab.
/// A zooms its pane; B keeps its tiled view, its own focus, and its own pane on
/// screen. Zoom is a fact about a view, not about the tab.
#[test]
fn one_clients_zoom_leaves_another_clients_view_alone() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id_a = ClientId::new();
    let client_id_b = ClientId::new();
    let tab_id = TabId::new();
    let (focused_pane_id, other_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, focused_pane_id);
    register_pane_record(&mut session, other_pane_id);
    register_session_tab(&mut session, tab_id, focused_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(build_horizontal_split(focused_pane_id, other_pane_id));
    attach_client(&mut session, client_id_a, tab_id, Some(focused_pane_id));
    attach_client(&mut session, client_id_b, tab_id, Some(other_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Client A zooms the pane it has focused.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id_a),
        Command::TogglePaneFullscreen,
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id_a, tab_id),
        LayoutMode::Fullscreen { focused_pane_id },
        "the client that zoomed sees its pane filling the tab"
    );
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id_b, tab_id),
        LayoutMode::Tiled,
        "the other client keeps its tiled view: another client's zoom is not its business"
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id_b)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(other_pane_id),
        "and keeps its own focus"
    );

    // B re-focuses the pane it already holds: nothing about its view changed, so
    // nothing at all happens.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id_b),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(other_pane_id),
            client_id: None,
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(emitted_events, Vec::new());
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id_b, tab_id),
        LayoutMode::Tiled
    );
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
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id_a,
        root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);

    // A second client views the same tab, tiled, focused on the other pane.
    let client_id_b = ClientId::new();
    attach_client(
        runtime.session_by_id.get_mut(&session_id).expect("session"),
        client_id_b,
        tab_id,
        Some(root_pane_id),
    );

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id_a),
        Command::TogglePaneFullscreen,
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id_a, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: pane_id_a
        }
    );
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id_b, tab_id),
        LayoutMode::Tiled
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&pane_id_a], pane_pty_size_a,
        "client B still draws pane_a tiled, so its child keeps the size B can show"
    );
}

/// The tiled client leaving takes its claim on the pane's size with it: the pane
/// is now drawn only by the client that has it zoomed, so the child finally grows
/// to fill the tab. The size follows the viewers who are actually looking.
#[test]
fn a_zoomed_pane_grows_once_the_client_holding_it_tiled_detaches() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id_a,
        root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);

    let client_id_b = ClientId::new();
    attach_client(
        runtime.session_by_id.get_mut(&session_id).expect("session"),
        client_id_b,
        tab_id,
        Some(root_pane_id),
    );

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id_a),
        Command::TogglePaneFullscreen,
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        runtime.pty_size_by_pane_id[&pane_id_a], pane_pty_size_a,
        "while B still shows it tiled, the child keeps the tiled size"
    );

    // B detaches. A, still zoomed, is the only client drawing pane_a.
    runtime.handle_client_detach(client_id_b);

    assert_eq!(
        runtime.pty_size_by_pane_id[&pane_id_a],
        PtySize {
            column_count: 78,
            row_count: 20
        },
        "with no tiled viewer left, the zoom finally gives the child the whole tab"
    );
}

/// A zoomed client focusing another pane swaps what its zoom shows — the zoomed
/// view changes content and stays on, and the tab's tree is never rewritten.
#[test]
fn focusing_another_pane_while_zoomed_swaps_what_the_zoom_shows() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (focused_pane_id, other_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, focused_pane_id);
    register_pane_record(&mut session, other_pane_id);
    register_session_tab(&mut session, tab_id, focused_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(build_horizontal_split(focused_pane_id, other_pane_id));
    attach_client(&mut session, client_id, tab_id, Some(focused_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    let tree_before = runtime.session_by_id[&session_id].tabs[&tab_id]
        .get_layout_tree()
        .clone();

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::TogglePaneFullscreen,
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    // Focus the pane the zoom is hiding: the zoom follows the focus onto it.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(other_pane_id),
            client_id: None,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: other_pane_id,
        },
        "the zoom moved to the newly focused pane"
    );
    assert_eq!(
        *runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        tree_before,
        "a zoom is a solve-time overlay: the tree is untouched"
    );
}

#[test]
fn new_pane_drops_the_splitting_clients_zoom() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_fullscreen_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);

    // Splitting the promoted pane: the tab returns to the tiled view and
    // both halves of the split are sized against it (root 40, the split
    // pair 20 each -> 18x20 content after chrome rows).
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Tiled
    );
    let session = &runtime.session_by_id[&session_id];
    let pane_id_b = session
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != pane_id_a && runtime.pty_size_by_pane_id.contains_key(pane_id))
        .expect("the new pane");
    let half = PtySize {
        column_count: 18,
        row_count: 20,
    };
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], half);
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_b], half);
}

#[test]
fn resize_pane_drops_the_resizing_clients_zoom() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_fullscreen_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);

    // The moved border must be visible: the resize lands in the tiled view.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Left,
            resize_amount_cells: 5,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Tiled
    );
    let expected_pty_size = PtySize {
        column_count: pane_pty_size_a.column_count + 5,
        row_count: pane_pty_size_a.row_count,
    };
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], expected_pty_size);
}

#[test]
fn close_pane_drops_the_closing_clients_zoom() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_fullscreen_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);
    let fullscreen_pty_size = PtySize {
        column_count: 78,
        row_count: 20,
    };

    // Closing the hidden root: the survivor already fills the tab, so its
    // PTY keeps the full-tab size it holds.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(root_pane_id),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Tiled
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        *session.tabs[&tab_id].get_layout_tree(),
        LayoutNode::Pane(pane_id_a)
    );
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], fullscreen_pty_size);
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(pane_id_a)
    );
}

#[test]
fn close_pane_closing_the_promoted_pane_drops_the_fullscreen() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_fullscreen_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(pane_id_a),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Tiled
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        *session.tabs[&tab_id].get_layout_tree(),
        LayoutNode::Pane(root_pane_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(root_pane_id)
    );
}

/// The (rows, cols) of `pane`'s terminal-engine grid.
fn get_terminal_engine_dimensions(runtime: &Server, pane_id: PaneId) -> (u16, u16) {
    runtime.list_terminal_engines()[&pane_id]
        .get_terminal_state()
        .get_active_grid()
        .get_grid_dimensions()
}

#[test]
fn new_pane_installs_a_terminal_engine_at_spawn_size() {
    let (
        runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        _client_id,
        root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();

    assert!(runtime.list_terminal_engines().contains_key(&pane_id_a));
    assert_eq!(
        get_terminal_engine_dimensions(&runtime, pane_id_a),
        (pane_pty_size_a.row_count, pane_pty_size_a.column_count),
        "engine grid matches the spawned PTY size"
    );
    // The fixture's root pane never spawned a PTY, so it has no engine.
    assert!(!runtime.list_terminal_engines().contains_key(&root_pane_id));
}

#[test]
fn close_pane_removes_the_terminal_engine() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();
    assert!(runtime.list_terminal_engines().contains_key(&pane_id_a));

    // No explicit pane: the focused (split) pane closes and its engine goes
    // with its PTY bookkeeping.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert!(!runtime.list_terminal_engines().contains_key(&pane_id_a));
}

#[test]
fn new_tab_installs_a_terminal_engine_at_spawn_size() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    assert!(runtime.list_terminal_engines().contains_key(&new_pane_id));
    assert_eq!(
        get_terminal_engine_dimensions(&runtime, new_pane_id),
        (
            runtime.pty_size_by_pane_id[&new_pane_id].row_count,
            runtime.pty_size_by_pane_id[&new_pane_id].column_count,
        ),
        "engine grid matches the spawned PTY size"
    );
}

#[test]
fn close_tab_removes_the_terminal_engines_of_its_panes() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // A second tab whose single pane spawned an engine.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);
    assert!(runtime.list_terminal_engines().contains_key(&new_pane_id));

    // Closing the client's active tab (the new one) drops its pane's engine.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs::default()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert!(!runtime.list_terminal_engines().contains_key(&new_pane_id));
}

#[test]
fn resize_pane_resizes_the_terminal_engine_with_its_pty() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Left,
            resize_amount_cells: 5,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    // The reflow resized A's PTY by 5 columns; its engine grid follows.
    let expected_pty_size = PtySize {
        column_count: pane_pty_size_a.column_count + 5,
        row_count: pane_pty_size_a.row_count,
    };
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], expected_pty_size);
    assert_eq!(
        get_terminal_engine_dimensions(&runtime, pane_id_a),
        (expected_pty_size.row_count, expected_pty_size.column_count)
    );
}

#[test]
fn child_exit_close_on_exit_removes_the_pane_and_reaps_it() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);

    // The split pane's `CloseOnExit` child dies.
    let events = runtime.handle_child_exit(new_pane_id, ExitStatus::ExitCode(0));

    // The exit is reported first, carrying the pane and its code.
    match events.first() {
        Some(Event::PaneProcessExited(exited)) => {
            assert_eq!(exited.pane_id, new_pane_id);
            assert_eq!(exited.exit_code, Some(0));
        }
        other => panic!("expected PaneProcessExited first, got {other:?}"),
    }

    // The pane is gone from state and from every runtime map.
    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .is_none());
    assert!(!runtime.pty_handle_by_pane_id.contains_key(&new_pane_id));
    assert!(!runtime.pty_size_by_pane_id.contains_key(&new_pane_id));
    assert!(!runtime
        .terminal_engine_by_pane_id
        .contains_key(&new_pane_id));

    // The already-dead child is only reaped: the backend entry is released via
    // a single inline `Force` kill (the `exited` guard sends no signal).
    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(new_pane_id)
            .unwrap(),
        vec![KillPolicy::Force]
    );

    // The root survives and reclaims the whole tab.
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(root_pane_id)
    );
}

#[test]
fn child_exit_of_the_last_pane_closes_the_tab_and_quits() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let events = runtime.handle_child_exit(root_pane_id, ExitStatus::ExitCode(0));

    // Removing the last pane closes the tab, and closing the last tab quits.
    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(root_pane_id)
        .is_none());
    assert!(runtime.session_by_id[&session_id].tabs.is_empty());
    assert_eq!(
        list_event_names(&events),
        [
            "PaneProcessExited",
            "PaneClosing",
            "PaneRemoved",
            "TabClosed",
            "Quit"
        ]
    );
}

#[test]
fn child_exit_empties_a_tab_and_moves_the_viewer_to_a_sibling() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id_a = TabId::new();
    let tab_id_b = TabId::new();
    let pane_id_a = PaneId::new();
    let pane_id_b = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id, tab_id_a, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The sole pane of tab A exits: tab A closes, but tab B survives, so the
    // session does not quit and the viewer moves to tab B (which reflows).
    let events = runtime.handle_child_exit(pane_id_a, ExitStatus::ExitCode(0));

    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(pane_id_a)
        .is_none());
    assert!(!runtime.session_by_id[&session_id]
        .tabs
        .contains_key(&tab_id_a));
    assert!(runtime.session_by_id[&session_id]
        .tabs
        .contains_key(&tab_id_b));
    assert_eq!(
        list_event_names(&events),
        [
            "PaneProcessExited",
            "PaneClosing",
            "PaneRemoved",
            "TabClosed",
            "TabFocused",
            "PaneFocused"
        ]
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        tab_id_b
    );
}

#[test]
fn child_exit_of_an_unknown_pane_is_dropped() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // No session owns the pane (closed while its exit waited in the inbox).
    let events = runtime.handle_child_exit(PaneId::new(), ExitStatus::ExitCode(0));

    assert!(events.is_empty());
}

// The root pane exits with a code, then the last pane is killed by a signal:
// each exit carries exactly one of `exit_code` and `signal`, and the quit the
// last exit causes names the tab and carries that exit.
#[test]
fn child_exit_by_signal_reports_the_signal_and_the_quit_it_causes_carries_it() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, root_pane_id);

    let coded_exit_events = runtime.handle_child_exit(root_pane_id, ExitStatus::ExitCode(3));
    assert_eq!(
        coded_exit_events.first(),
        Some(&Event::PaneProcessExited(PaneProcessExited {
            pane_id: root_pane_id,
            exit_code: Some(3),
            signal: None,
        }))
    );
    assert!(
        !coded_exit_events
            .iter()
            .any(|event| matches!(event, Event::Quit(_))),
        "one pane survives, so the session does not quit: {coded_exit_events:?}"
    );

    let signaled_exit = PaneProcessExited {
        pane_id: new_pane_id,
        exit_code: None,
        signal: Some(9),
    };
    let signaled_exit_events = runtime.handle_child_exit(new_pane_id, ExitStatus::Signaled(9));

    assert_eq!(
        signaled_exit_events.first(),
        Some(&Event::PaneProcessExited(signaled_exit))
    );
    assert_eq!(
        signaled_exit_events.last(),
        Some(&Event::Quit(QuitCause::LastTabClosed {
            tab_id,
            pane_exit: Some(signaled_exit),
        }))
    );
}

/// The `(session, tab, pane)` of a runtime `bootstrap_local` built: exactly one
/// of each. Panics unless the runtime holds a single session with a single tab
/// and a single pane.
fn get_only_session_slot(runtime: &Server) -> (SessionId, TabId, PaneId) {
    let session = runtime
        .session_by_id
        .values()
        .next()
        .expect("exactly one session");
    let tab_id = *session.tabs.keys().next().expect("exactly one tab");
    let pane_id = session
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .next()
        .expect("exactly one pane");
    (session.session_id, tab_id, pane_id)
}

#[test]
fn client_attach_reflows_the_shared_tab_to_the_smaller_effective_size() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let large_viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let small_viewport_size = Size {
        column_count: 40,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), large_viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, pane_id) = get_only_session_slot(&runtime);
    let resize_count_before_attach = fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("pane spawned")
        .len();

    // A smaller second client attaches to the same tab: the effective size drops
    // to the per-axis minimum, so the live pane's PTY reflows down.
    let joining_client_id = ClientId::new();
    let events = runtime.handle_client_attach(
        session_id,
        joining_client_id,
        small_viewport_size,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );

    let expected_pty_size = size_root_pane(
        pane_id,
        pane_viewport(small_viewport_size),
        PaneSizing::default(),
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).unwrap().len(),
        resize_count_before_attach + 1
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(pane_id)
            .unwrap()
            .last()
            .unwrap(),
        expected_pty_size
    );
    // The joining client had focused nothing here, so it lands on the tab's
    // pane before the reflow it caused.
    assert_eq!(
        events,
        vec![
            Event::PaneFocused(PaneFocused {
                client_id: joining_client_id,
                tab_id,
                pane_id,
                previous_pane_id: None,
            }),
            Event::PtyResized(PtyResized {
                pane_id,
                pty_size: expected_pty_size,
            })
        ]
    );
}

#[test]
fn attach_applies_cell_measurement_before_reflow_and_resize_can_clear_it() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport = Size {
        column_count: 80,
        row_count: 24,
    };
    let _bootstrap_client_id = runtime
        .bootstrap_local(SessionId::new(), viewport, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);
    let measurement =
        PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");
    let measured_id = ClientId::new();

    runtime.handle_client_attach_with_cell_size(
        session_id,
        measured_id,
        viewport,
        None,
        tab_id,
        Some(measurement),
        SystemTime::now(),
        false,
    );
    let measured_client = runtime.session_by_id[&session_id]
        .clients
        .list_attached_clients()
        .find(|candidate| candidate.get_client_id() == measured_id)
        .expect("the measured client attached");
    assert_eq!(measured_client.get_cell_size(), Some(measurement));
    assert_eq!(
        runtime.session_by_id[&session_id].get_tab_cell_size(tab_id),
        Some(measurement)
    );

    runtime.handle_client_resize_with_cell_size(measured_id, viewport, None, None);
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(measured_id)
            .expect("the measured client remains attached")
            .get_cell_size(),
        None
    );
    assert_eq!(
        runtime.session_by_id[&session_id].get_tab_cell_size(tab_id),
        None,
        "clearing the only measured viewer leaves no shared measurement"
    );
}

#[test]
fn accepting_cell_measurement_invalidates_the_next_frame() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport = Size {
        column_count: 80,
        row_count: 24,
    };
    let client = runtime
        .bootstrap_local(SessionId::new(), viewport, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (_session_id, _tab_id, _pane_id) = get_only_session_slot(&runtime);
    let measurement =
        PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");

    let rendered_at = Instant::now();
    assert!(runtime.render_scheduler.poll(rendered_at));
    runtime.handle_client_cell_size(client, measurement);

    assert!(
        runtime
            .render_scheduler
            .next_wakeup(Instant::now())
            .is_some(),
        "the accepted measurement schedules the next frame"
    );
}

#[test]
fn client_resize_updates_full_viewport_and_reflows_middle_pane_region() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let initial_viewport = Size {
        column_count: 80,
        row_count: 24,
    };
    let resized_viewport = Size {
        column_count: 100,
        row_count: 30,
    };
    let client = runtime
        .bootstrap_local(SessionId::new(), initial_viewport, SystemTime::now())
        .expect("bootstrap");
    let (_session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);

    let events = runtime.handle_client_resize(client, resized_viewport, None);
    let expected_pty_size = size_root_pane(
        pane_id,
        pane_viewport(resized_viewport),
        PaneSizing::default(),
    );

    assert_eq!(
        runtime
            .get_session_for_client(client)
            .unwrap()
            .clients
            .get_client_by_id(client)
            .unwrap()
            .get_viewport_size(),
        resized_viewport
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(pane_id)
            .unwrap()
            .last()
            .unwrap(),
        expected_pty_size
    );
    assert_eq!(
        events,
        vec![Event::PtyResized(PtyResized {
            pane_id,
            pty_size: expected_pty_size,
        })]
    );
}

#[test]
fn client_attach_of_a_larger_client_leaves_the_tab_size_unchanged() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let small_viewport_size = Size {
        column_count: 40,
        row_count: 24,
    };
    let large_viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), small_viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, pane_id) = get_only_session_slot(&runtime);
    let resize_count_before_attach = fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("pane spawned")
        .len();

    // The larger client cannot lower the per-axis minimum, so the effective size
    // stays 40x24: no reflow, no resize event.
    let joining_client_id = ClientId::new();
    let events = runtime.handle_client_attach(
        session_id,
        joining_client_id,
        large_viewport_size,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );

    // No reflow, but the joining client still lands on the tab's pane.
    assert_eq!(
        events,
        vec![Event::PaneFocused(PaneFocused {
            client_id: joining_client_id,
            tab_id,
            pane_id,
            previous_pane_id: None,
        })]
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).unwrap().len(),
        resize_count_before_attach
    );
}

#[test]
fn attaching_to_a_session_seeded_with_no_client_lands_on_the_tabs_first_pane() {
    // Exactly what a session server started with nothing attached holds: one
    // tab and one pane, and no client to have focused anything.
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport = Size {
        column_count: 80,
        row_count: 24,
    };
    let session_id = SessionId::new();
    runtime
        .bootstrap_session(
            session_id,
            "seeded-with-no-client".to_string(),
            viewport,
            SystemTime::now(),
            None,
        )
        .expect("bootstrap a session holding no client");
    let (_, tab_id, pane_id) = get_only_session_slot(&runtime);
    assert!(
        runtime.session_by_id[&session_id]
            .clients
            .list_attached_clients()
            .next()
            .is_none(),
        "the session starts with no client"
    );

    let client_id = ClientId::new();
    let events = runtime.handle_client_attach(
        session_id,
        client_id,
        viewport,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );

    // The attaching client lands focused on the tab's only pane, so the first
    // key it types reaches that pane's shell.
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .expect("the client attached")
            .get_focused_pane(tab_id),
        Some(pane_id)
    );
    assert_eq!(
        events,
        vec![Event::PaneFocused(PaneFocused {
            client_id,
            tab_id,
            pane_id,
            previous_pane_id: None,
        })]
    );
}

#[test]
fn reattaching_keeps_the_pane_the_client_already_focused() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), viewport, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, pane_id) = get_only_session_slot(&runtime);

    // Attach once to take the tab's first pane, then split so the tab holds a
    // second pane and focus moves onto it.
    let client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        client_id,
        viewport,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );
    let newly_focused_pane_id = PaneId::new();
    runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client")
        .update_focused_pane(tab_id, newly_focused_pane_id);

    // Re-attaching does not drag focus back to the tab's first pane.
    let events = runtime.handle_client_attach(
        session_id,
        client_id,
        viewport,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );

    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .expect("the client is still attached")
            .get_focused_pane(tab_id),
        Some(newly_focused_pane_id),
        "a client that already focused a pane keeps it"
    );
    assert_ne!(newly_focused_pane_id, pane_id);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::PaneFocused(_))),
        "no focus change is announced, got {events:?}"
    );
}

#[test]
fn client_detach_reflows_the_shared_tab_back_to_the_remaining_viewport() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let large_viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let small_viewport_size = Size {
        column_count: 40,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), large_viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, pane_id) = get_only_session_slot(&runtime);

    // Two clients view the tab; the smaller one holds it at 40x24.
    let small_client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        small_client_id,
        small_viewport_size,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );
    let resize_count_before_detach = fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("pane spawned")
        .len();

    // The smaller client leaves: only the 80x24 viewer remains, so the tab grows
    // back and the pane's PTY reflows up.
    let events = runtime.handle_client_detach(small_client_id);

    let expected_pty_size = size_root_pane(
        pane_id,
        pane_viewport(large_viewport_size),
        PaneSizing::default(),
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).unwrap().len(),
        resize_count_before_detach + 1
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(pane_id)
            .unwrap()
            .last()
            .unwrap(),
        expected_pty_size
    );
    assert_eq!(
        events,
        vec![Event::PtyResized(PtyResized {
            pane_id,
            pty_size: expected_pty_size,
        })]
    );
}

#[test]
fn last_client_detach_keeps_pty_sizes_and_emits_no_resize() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let client = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);
    let resize_count_before_last_detach = fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("pane spawned")
        .len();

    // The only viewer leaves: the tab has no viewport, so its PTY keeps its size
    // and no resize event is produced. The pane itself stays alive.
    let events = runtime.handle_client_detach(client);

    assert!(events.is_empty());
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).unwrap().len(),
        resize_count_before_last_detach
    );
    assert!(runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client)
        .is_none());
    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(pane_id)
        .is_some());
}

#[test]
fn a_client_leaving_does_not_end_the_session_by_default() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);

    // `auto-close-session` defaults off, so the session and its pane outlive the
    // last client.
    runtime.handle_client_detach(client);

    assert!(!runtime.is_quit_requested());
    assert!(!runtime.should_shutdown_immediately);
    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(pane_id)
        .is_some());
}

#[test]
fn auto_close_keeps_the_session_while_another_client_is_still_attached() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let initial_client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);
    runtime.config.should_auto_close_session = true;

    let additional_client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        additional_client_id,
        viewport_size,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );

    // One of two clients leaves: one is still attached, so nothing quits.
    runtime.handle_client_detach(initial_client_id);

    assert!(!runtime.is_quit_requested());
    assert_eq!(runtime.session_by_id[&session_id].clients.client_count(), 1);
}

#[test]
fn auto_close_ends_the_session_when_the_last_client_leaves() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    runtime.config.should_auto_close_session = true;

    runtime.handle_client_detach(client);

    assert!(runtime.is_quit_requested());
    // Teardown asks each pane's child to stop and waits before killing it, the
    // branch `Server::shutdown` takes while `immediate_shutdown` is unset.
    assert!(!runtime.should_shutdown_immediately);
}

#[test]
fn a_quit_command_tears_down_without_the_graceful_window() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");

    runtime.request_quit();

    assert!(runtime.is_quit_requested());
    assert!(runtime.should_shutdown_immediately);
}

#[test]
fn auto_close_ends_the_session_when_detach_all_empties_it() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);
    runtime.config.should_auto_close_session = true;
    runtime.handle_client_attach(
        session_id,
        ClientId::new(),
        viewport_size,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );
    assert_eq!(runtime.session_by_id[&session_id].clients.client_count(), 2);

    // `DetachAll` runs the same per-client departure the setting watches, so the
    // pass that removes the last one ends the session.
    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::DetachAll,
    );
    let _ = runtime.dispatch(command_envelope);

    assert_eq!(runtime.session_by_id[&session_id].clients.client_count(), 0);
    assert!(runtime.is_quit_requested());
    assert!(!runtime.should_shutdown_immediately);
}

#[test]
fn detach_all_leaves_the_session_running_while_auto_close_is_off() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);
    runtime.handle_client_attach(
        session_id,
        ClientId::new(),
        viewport_size,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );

    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::DetachAll,
    );
    let _ = runtime.dispatch(command_envelope);

    assert_eq!(runtime.session_by_id[&session_id].clients.client_count(), 0);
    assert!(!runtime.is_quit_requested());
}

#[test]
fn auto_close_leaves_a_headless_session_with_no_clients_running() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    runtime.config.should_auto_close_session = true;

    // A headless start seeds the session with no client at all.
    runtime
        .bootstrap_session(
            SessionId::new(),
            "s".to_string(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
            None,
        )
        .expect("bootstrap the headless session");
    assert!(!runtime.is_quit_requested());

    // A detach for a client this runtime never held is dropped, so the
    // already-empty session is not closed by it either.
    runtime.handle_client_detach(ClientId::new());
    assert!(!runtime.is_quit_requested());
}

#[test]
fn a_detach_command_ends_the_session_under_auto_close() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);
    runtime.config.should_auto_close_session = true;

    // The client detaches itself: the command path lands in the same handler a
    // connection drop does.
    let command_envelope = build_command_envelope(
        CommandSource::from_in_session_cli(
            session_id,
            Some(client),
            pane_id,
            PathBuf::from("/sock"),
        ),
        Command::Detach(DetachArgs { client_id: None }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.submit_command(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
    assert!(runtime.is_quit_requested());
}

#[test]
fn quit_from_a_client_detaches_it_and_leaves_the_session_running() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);

    // `auto-close-session` is off, so quit removes the client and nothing else:
    // the session and its pane keep running.
    let command_envelope =
        build_command_envelope(CommandSource::from_key_binding(client), Command::Quit);
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.submit_command(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
    assert_eq!(runtime.session_by_id[&session_id].clients.client_count(), 0);
    assert!(!runtime.is_quit_requested());
    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(pane_id)
        .is_some());
}

#[test]
fn quit_from_one_of_two_clients_keeps_the_session_under_auto_close() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let initial_client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);
    runtime.config.should_auto_close_session = true;
    let additional_client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        additional_client_id,
        viewport_size,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );

    // One of two clients quits: the other is still attached, so nothing quits.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(initial_client_id),
        Command::Quit,
    );
    assert!(matches!(
        runtime.submit_command(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert!(runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(initial_client_id)
        .is_none());
    assert!(runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(additional_client_id)
        .is_some());
    assert!(!runtime.is_quit_requested());
}

#[test]
fn quit_from_the_last_client_ends_the_session_under_auto_close() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, _pane_id) = get_only_session_slot(&runtime);
    runtime.config.should_auto_close_session = true;

    let command_envelope =
        build_command_envelope(CommandSource::from_key_binding(client), Command::Quit);
    assert!(matches!(
        runtime.submit_command(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(runtime.session_by_id[&session_id].clients.client_count(), 0);
    assert!(runtime.is_quit_requested());
    // The departure is an ordinary detach, so teardown keeps the graceful
    // window the way every other auto-close departure does.
    assert!(!runtime.should_shutdown_immediately);
}

#[test]
fn quit_from_a_client_that_already_left_is_refused() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, _pane_id) = get_only_session_slot(&runtime);
    runtime.config.should_auto_close_session = true;

    // A keypress from a client this runtime no longer holds locates no session,
    // so it cannot end one.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(ClientId::new()),
        Command::Quit,
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.submit_command(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: None,
        }
    );
    assert_eq!(runtime.session_by_id[&session_id].clients.client_count(), 1);
    assert!(!runtime.is_quit_requested());
}

#[test]
fn client_attach_to_an_unknown_session_is_dropped() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let events = runtime.handle_client_attach(
        SessionId::new(),
        ClientId::new(),
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        TabId::new(),
        SystemTime::now(),
        false,
    );
    assert!(events.is_empty());
}

#[test]
fn client_detach_of_an_unknown_client_is_dropped() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let events = runtime.handle_client_detach(ClientId::new());
    assert!(events.is_empty());
}

#[test]
fn client_attach_to_an_unknown_tab_is_dropped() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);
    let resize_count_before_unknown_tab_attach = fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("pane spawned")
        .len();

    // The named tab is not one this session holds: the client is not attached
    // and nothing reflows.
    let stranger_client_id = ClientId::new();
    let events = runtime.handle_client_attach(
        session_id,
        stranger_client_id,
        viewport_size,
        None,
        TabId::new(),
        SystemTime::now(),
        false,
    );

    assert!(events.is_empty());
    assert!(runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(stranger_client_id)
        .is_none());
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).unwrap().len(),
        resize_count_before_unknown_tab_attach
    );
}

#[test]
fn client_reattach_onto_a_different_tab_reflows_the_tab_it_left() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let large_viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let small_viewport_size = Size {
        column_count: 40,
        row_count: 24,
    };
    let client_id_a = runtime
        .bootstrap_local(SessionId::new(), large_viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id_1, pane_id_1) = get_only_session_slot(&runtime);

    // A second live tab: `NewTab` moves client A onto it, leaving `tab_1` with
    // no viewer and `pane_1` at its bootstrap size.
    match runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id_a),
        Command::NewTab(NewTabArgs::default()),
    )) {
        CommandResult::Ok { .. } => {}
        other => panic!("expected Ok, got {other:?}"),
    }
    let tab_id_2 = runtime.session_by_id[&session_id]
        .tabs
        .values()
        .find(|tab_id| tab_id.get_tab_id() != tab_id_1)
        .expect("the created tab")
        .get_tab_id();

    // Two clients view `tab_1`; the smaller one (C) constrains `pane_1` to 40x24.
    let client_id_b = ClientId::new();
    let client_c = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        client_id_b,
        large_viewport_size,
        None,
        tab_id_1,
        SystemTime::now(),
        false,
    );
    runtime.handle_client_attach(
        session_id,
        client_c,
        small_viewport_size,
        None,
        tab_id_1,
        SystemTime::now(),
        false,
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(pane_id_1)
            .unwrap()
            .last()
            .unwrap(),
        size_root_pane(
            pane_id_1,
            pane_viewport(small_viewport_size),
            PaneSizing::default(),
        )
    );
    let resize_count_before_tab_reattach =
        fake_pty_backend.list_pane_sizes(pane_id_1).unwrap().len();

    // C re-attaches onto `tab_2`: it leaves `tab_1`, where only the 80x24 client
    // B remains, so `pane_1` grows back — the tab the client left is reflowed.
    let events = runtime.handle_client_attach(
        session_id,
        client_c,
        large_viewport_size,
        None,
        tab_id_2,
        SystemTime::now(),
        false,
    );

    let expected_pty_size = size_root_pane(
        pane_id_1,
        pane_viewport(large_viewport_size),
        PaneSizing::default(),
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id_1).unwrap().len(),
        resize_count_before_tab_reattach + 1
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(pane_id_1)
            .unwrap()
            .last()
            .unwrap(),
        expected_pty_size
    );
    // `client_c` moved onto `tab_2`, where it had focused nothing, so it lands
    // on that tab's pane; the reflow it caused on `tab_1` follows.
    let pane_id_2 = runtime.session_by_id[&session_id].tabs[&tab_id_2]
        .get_layout_tree()
        .list_leaf_pane_ids()
        .first()
        .copied()
        .expect("the created tab holds one pane");
    assert_eq!(
        events,
        vec![
            Event::PaneFocused(PaneFocused {
                client_id: client_c,
                tab_id: tab_id_2,
                pane_id: pane_id_2,
                previous_pane_id: None,
            }),
            Event::PtyResized(PtyResized {
                pane_id: pane_id_1,
                pty_size: expected_pty_size,
            })
        ]
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_c)
            .unwrap()
            .get_active_tab(),
        tab_id_2
    );
}

#[test]
fn client_attach_schedules_a_render() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);

    // Drain the render the bootstrap scheduled, so a fresh render is due only if
    // the attach schedules one.
    let now = Instant::now();
    assert!(runtime.poll_render(now));
    assert!(!runtime.poll_render(now));

    // A larger client attaches: it cannot shrink the tab, so no PTY reflows —
    // but the new viewer still needs a frame.
    runtime.handle_client_attach(
        session_id,
        ClientId::new(),
        Size {
            column_count: 120,
            row_count: 40,
        },
        None,
        tab_id,
        SystemTime::now(),
        false,
    );

    assert!(runtime.poll_render(now + Duration::from_secs(1)));
}

#[test]
fn client_detach_schedules_a_render() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");

    // Drain the render the bootstrap scheduled.
    let now = Instant::now();
    assert!(runtime.poll_render(now));
    assert!(!runtime.poll_render(now));

    // The last viewer leaves: no PTY reflows, but the detach still schedules a
    // render.
    runtime.handle_client_detach(client);

    assert!(runtime.poll_render(now + Duration::from_secs(1)));
}

#[test]
fn unviewed_tab_adoption_sizes_the_new_pane_to_the_pane_region() {
    let viewport = Size {
        column_count: 100,
        row_count: 40,
    };

    // Baseline: the same-sized client splits the tab it already views, so the
    // solve runs against the tab's drawable pane region.
    let (mut rt_viewed, fake_viewed, _tx_viewed) = build_runtime_with_fake();
    let client_viewed = ClientId::new();
    let viewed_tab_id = TabId::new();
    let viewed_root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, viewed_root_pane_id);
    register_session_tab(&mut session, viewed_tab_id, viewed_root_pane_id);
    let mut client = Client::from_attachment(
        client_viewed,
        session.session_id,
        SystemTime::now(),
        viewport,
        None,
        viewed_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(viewed_tab_id, viewed_root_pane_id);
    session.attach_client(client);
    let session_id_viewed = session.session_id;
    rt_viewed.session_by_id.insert(session_id_viewed, session);
    rt_viewed.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_viewed),
        Command::NewPane(build_new_pane_args()),
    ));
    let baseline_pane_id = find_other_pane_id(&rt_viewed, session_id_viewed, viewed_root_pane_id);
    let baseline_size = fake_viewed
        .list_pane_sizes(baseline_pane_id)
        .expect("baseline pane spawned")[0];

    // Adoption: an identical client is designated onto an UNVIEWED tab. The
    // new pane must be fit and spawned against the client's pane region — the
    // same geometry as the viewed baseline — not the full terminal viewport.
    let (mut rt_adopt, fake_adopt, _tx_adopt) = build_runtime_with_fake();
    let client_adopt = ClientId::new();
    let front_tab_id = TabId::new();
    let back_tab_id = TabId::new();
    let front_pane_id = PaneId::new();
    let back_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_pane_id);
    register_pane_record(&mut session, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);
    let mut client = Client::from_attachment(
        client_adopt,
        session.session_id,
        SystemTime::now(),
        viewport,
        None,
        front_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(front_tab_id, front_pane_id);
    session.attach_client(client);
    let session_id_adopt = session.session_id;
    rt_adopt.session_by_id.insert(session_id_adopt, session);
    rt_adopt.dispatch(build_command_envelope(
        CommandSource::from_external_cli(Some(session_id_adopt), None),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(back_pane_id),
            client_id: Some(client_adopt),
            ..build_new_pane_args()
        }),
    ));
    let adopted_pane_id = rt_adopt.session_by_id[&session_id_adopt]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != front_pane_id && *pane_id != back_pane_id)
        .expect("the adopted split pane");
    let adopted_size = fake_adopt
        .list_pane_sizes(adopted_pane_id)
        .expect("adopted pane spawned")[0];

    assert_eq!(adopted_size, baseline_size);
}

#[test]
fn dispatched_command_schedules_a_render() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);

    // Drain the render the bootstrap scheduled.
    let now = Instant::now();
    assert!(runtime.poll_render(now));
    assert!(!runtime.poll_render(now));

    // A command arriving outside the key path — the IPC shape — mutates the
    // layout; the dispatch itself must schedule the frame.
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(pane_id),
            ..build_new_pane_args()
        }),
    ));
    assert!(matches!(command_result, CommandResult::Ok { .. }));

    assert!(runtime.poll_render(now + Duration::from_secs(1)));
}

#[test]
fn same_session_reattach_preserves_client_view_state() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let initial_viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let client = runtime
        .bootstrap_local(SessionId::new(), initial_viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, pane_id) = get_only_session_slot(&runtime);

    // The client accumulated per-tab focus.
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .clients
        .get_client_mut_by_id(client)
        .unwrap()
        .update_focused_pane(tab_id, pane_id);

    // A re-attach of the same live id (e.g. a transport blip with no clean
    // detach) updates the view in place — it must not wipe accumulated state.
    let grown = Size {
        column_count: 100,
        row_count: 30,
    };
    runtime.handle_client_attach(
        session_id,
        client,
        grown,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );

    let client_record = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client)
        .expect("still attached");
    assert_eq!(client_record.get_focused_pane(tab_id), Some(pane_id));
    assert_eq!(client_record.get_viewport_size(), grown);
    assert_eq!(client_record.get_active_tab(), tab_id);
}

#[test]
fn cross_session_attach_detaches_the_client_from_its_old_session() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let large_viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let small_viewport_size = Size {
        column_count: 40,
        row_count: 24,
    };

    let client = runtime
        .bootstrap_local(SessionId::new(), large_viewport_size, SystemTime::now())
        .expect("first session");
    let (session_id_1, _tab_id_1, pane_id_1) = get_only_session_slot(&runtime);

    // A second, independent session with its own live pane.
    runtime
        .bootstrap_local(SessionId::new(), large_viewport_size, SystemTime::now())
        .expect("second session");
    let session_id_2 = *runtime
        .session_by_id
        .keys()
        .find(|&&candidate_session_id| candidate_session_id != session_id_1)
        .expect("the second session");
    let session_2 = &runtime.session_by_id[&session_id_2];
    let tab_id_2 = *session_2.tabs.keys().next().expect("its tab");
    let pane_id_2 = session_2
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .next()
        .expect("its pane");
    let pane_1_resizes_before = fake_pty_backend
        .list_pane_sizes(pane_id_1)
        .expect("pane spawned")
        .len();

    // Move `client` from session 1 into session 2 at a smaller viewport.
    let events = runtime.handle_client_attach(
        session_id_2,
        client,
        small_viewport_size,
        None,
        tab_id_2,
        SystemTime::now(),
        false,
    );

    // It left session 1 entirely and is now the 40x24 co-viewer of session 2.
    assert!(runtime.session_by_id[&session_id_1]
        .clients
        .get_client_by_id(client)
        .is_none());
    assert_eq!(
        runtime.session_by_id[&session_id_2]
            .clients
            .get_client_by_id(client)
            .expect("moved into session 2")
            .get_active_tab(),
        tab_id_2
    );

    // Session 2's pane shrinks to the new minimum; session 1's pane keeps its
    // size (its tab lost its only viewer).
    let expected_pty_size = size_root_pane(
        pane_id_2,
        pane_viewport(small_viewport_size),
        PaneSizing::default(),
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(pane_id_2)
            .unwrap()
            .last()
            .unwrap(),
        expected_pty_size
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id_1).unwrap().len(),
        pane_1_resizes_before
    );
    // The client had focused nothing in session 2, so it lands on that tab's
    // pane before the reflow its smaller viewport caused.
    assert_eq!(
        events,
        vec![
            Event::PaneFocused(PaneFocused {
                client_id: client,
                tab_id: tab_id_2,
                pane_id: pane_id_2,
                previous_pane_id: None,
            }),
            Event::PtyResized(PtyResized {
                pane_id: pane_id_2,
                pty_size: expected_pty_size,
            })
        ]
    );
}

// A split narrows its sibling: the sibling's terminal grid must re-wrap its
// content to the new width, not keep the old-width rows for the renderer to
// clip. Asserts on grid cells, not just the PtyResized event.
#[test]
fn new_pane_split_rewraps_the_sibling_grid_content() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let tab_id = TabId::new();
    let pane_id_a = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id, pane_id_a);
    let client_id = ClientId::new();
    attach_client(&mut session, client_id, tab_id, Some(pane_id_a));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // First split: pane_x gets a live PTY + engine at the two-pane width.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let pane_id_x = find_other_pane_id(&runtime, session_id, pane_id_a);
    let wide_pane_pty_size = runtime.pty_size_by_pane_id[&pane_id_x];
    let line: String = "A".repeat(wide_pane_pty_size.column_count as usize - 2);
    let _ = runtime
        .terminal_engine_by_pane_id
        .get_mut(&pane_id_x)
        .unwrap()
        .process_pty_output(line.as_bytes());

    // Second split of pane_x (it holds focus): pane_x narrows.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let narrow_pane_pty_size = runtime.pty_size_by_pane_id[&pane_id_x];
    assert!(
        narrow_pane_pty_size.column_count < wide_pane_pty_size.column_count,
        "narrow {narrow_pane_pty_size:?} wide {wide_pane_pty_size:?}"
    );
    let grid = runtime.terminal_engine_by_pane_id[&pane_id_x]
        .get_terminal_state()
        .get_active_grid();
    assert_eq!(
        grid.get_grid_dimensions(),
        (
            narrow_pane_pty_size.row_count,
            narrow_pane_pty_size.column_count
        )
    );
    let row0: String = grid.list_rows()[0]
        .iter()
        .map(koshi_terminal::grid::state::Cell::get_character)
        .collect();
    let row1: String = grid.list_rows()[1]
        .iter()
        .map(koshi_terminal::grid::state::Cell::get_character)
        .collect();
    let expect0 = "A".repeat(narrow_pane_pty_size.column_count as usize);
    let rest =
        wide_pane_pty_size.column_count as usize - 2 - narrow_pane_pty_size.column_count as usize;
    let expect1 = format!(
        "{}{}",
        "A".repeat(rest),
        " ".repeat(narrow_pane_pty_size.column_count as usize - rest)
    );
    assert_eq!(row0, expect0, "row0 must be a full wrapped slice");
    assert_eq!(row1, expect1, "row1 must carry the wrapped remainder");
}

// The genesis root pane goes through `bootstrap_local`, not the new-pane
// handler; its first split must still re-wrap the root's grid content.
#[test]
fn bootstrap_root_pane_rewraps_on_first_split() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap");
    let session_id = *runtime.session_by_id.keys().next().unwrap();
    let root_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .next()
        .unwrap();
    let wide_pane_pty_size = runtime.pty_size_by_pane_id[&root_pane_id];
    let line: String = "A".repeat(wide_pane_pty_size.column_count as usize - 2);
    let _ = runtime
        .terminal_engine_by_pane_id
        .get_mut(&root_pane_id)
        .unwrap()
        .process_pty_output(line.as_bytes());

    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client),
        Command::NewPane(build_new_pane_args()),
    ));
    let narrow_pane_pty_size = runtime.pty_size_by_pane_id[&root_pane_id];
    assert!(
        narrow_pane_pty_size.column_count < wide_pane_pty_size.column_count,
        "narrow {narrow_pane_pty_size:?} wide {wide_pane_pty_size:?}"
    );
    let grid = runtime.terminal_engine_by_pane_id[&root_pane_id]
        .get_terminal_state()
        .get_active_grid();
    assert_eq!(
        grid.get_grid_dimensions(),
        (
            narrow_pane_pty_size.row_count,
            narrow_pane_pty_size.column_count
        )
    );
    let row0: String = grid.list_rows()[0]
        .iter()
        .map(koshi_terminal::grid::state::Cell::get_character)
        .collect();
    assert_eq!(row0, "A".repeat(narrow_pane_pty_size.column_count as usize));
}

#[test]
fn pane_spawn_sizes_gives_each_pane_of_a_two_pane_tab_its_own_tile() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(left_pane_id),
            LayoutNode::Pane(right_pane_id),
        ],
    ));
    let viewport = Size {
        column_count: 80,
        row_count: 24,
    };

    // Each pane is sized to its 40-column half minus its one-cell border on
    // each side (38 content columns, 22 rows), not the whole 80-column tab.
    let sizes = compute_pane_spawn_sizes(&tree, viewport, PaneSizing::default());
    assert_eq!(
        sizes,
        vec![
            (
                left_pane_id,
                PtySize {
                    column_count: 38,
                    row_count: 22
                }
            ),
            (
                right_pane_id,
                PtySize {
                    column_count: 38,
                    row_count: 22
                }
            ),
        ]
    );

    // A single pane over the same viewport keeps the full inner width, so the
    // two-pane tiles really are narrower.
    assert_eq!(
        size_root_pane(left_pane_id, viewport, PaneSizing::default()),
        PtySize {
            column_count: 78,
            row_count: 22
        }
    );
}

#[test]
fn size_root_pane_falls_back_to_the_whole_viewport_for_a_suppressed_pane() {
    // A 3x3 viewport is below the pane's border-inclusive floor of 4 columns,
    // so the solve gives the pane no content rect and the size is taken from
    // the whole viewport rect instead.
    assert_eq!(
        size_root_pane(
            PaneId::new(),
            Size {
                column_count: 3,
                row_count: 3
            },
            PaneSizing::default()
        ),
        PtySize {
            column_count: 3,
            row_count: 3
        }
    );
}

#[test]
fn pane_spawn_sizes_falls_back_to_the_tab_rect_for_a_suppressed_pane() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(first_pane_id),
            LayoutNode::Pane(second_pane_id),
        ],
    ));

    // Neither half fits in a 3x3 viewport, so both panes are suppressed and
    // each falls back to the full 3x3 tab rect.
    assert_eq!(
        compute_pane_spawn_sizes(
            &tree,
            Size {
                column_count: 3,
                row_count: 3
            },
            PaneSizing::default()
        ),
        vec![
            (
                first_pane_id,
                PtySize {
                    column_count: 3,
                    row_count: 3
                }
            ),
            (
                second_pane_id,
                PtySize {
                    column_count: 3,
                    row_count: 3
                }
            ),
        ]
    );
}

#[test]
fn span_overlap_measures_only_the_length_two_spans_share() {
    // `[0, 10)` and `[4, 10)` share `[4, 10)`: six cells.
    assert_eq!(compute_span_overlap(0, 10, 4, 6), 6);
    // Full containment answers the inner span's whole length, either way round.
    assert_eq!(compute_span_overlap(0, 10, 2, 3), 3);
    assert_eq!(compute_span_overlap(2, 3, 0, 10), 3);
    // Identical spans overlap along their whole length.
    assert_eq!(compute_span_overlap(5, 4, 5, 4), 4);
    // `[0, 5)` and `[5, 10)` touch end to end and share no cell.
    assert_eq!(compute_span_overlap(0, 5, 5, 5), 0);
    // Disjoint spans share nothing, in either order.
    assert_eq!(compute_span_overlap(0, 2, 7, 3), 0);
    assert_eq!(compute_span_overlap(7, 3, 0, 2), 0);
    // A zero-length span shares nothing, even inside the other span.
    assert_eq!(compute_span_overlap(0, 10, 5, 0), 0);
}

#[test]
fn tab_focused_in_reports_the_first_focused_tab() {
    let (first, second) = (TabId::new(), TabId::new());
    let client_id = ClientId::new();
    let focused = |tab_id| {
        Event::TabFocused(koshi_core::event::TabFocused {
            client_id,
            tab_id,
            previous_tab_id: first,
        })
    };

    // Two switches in one batch: the first entry names the answer.
    assert_eq!(
        find_first_focused_tab_id(&[focused(first), focused(second)]),
        Some(first)
    );
    // A batch that holds no switch, and an empty batch, name none.
    assert_eq!(
        find_first_focused_tab_id(&[Event::Quit(QuitCause::Requested), Event::Restarting]),
        None
    );
    assert_eq!(find_first_focused_tab_id(&[]), None);
}

#[test]
fn build_default_shell_spec_uses_the_configured_shell_and_terminal_identity() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    runtime.config.terminal.default_shell = Some("/opt/homebrew/bin/fish".to_string());
    runtime.config.terminal.term = "xterm-kitty".to_string();
    runtime.config.terminal.colorterm = "24bit".to_string();

    let spec = runtime.build_default_shell_spec(None, BTreeMap::new());
    assert_eq!(spec.program, PathBuf::from("/opt/homebrew/bin/fish"));
    assert_eq!(spec.shell_kind, ShellKind::Fish);
    assert_eq!(
        spec.environment_variables.get("TERM").map(String::as_str),
        Some("xterm-kitty")
    );
    assert_eq!(
        spec.environment_variables
            .get("COLORTERM")
            .map(String::as_str),
        Some("24bit")
    );
}

#[test]
fn apply_terminal_identity_environment_variables_preserves_a_panes_own_value() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    runtime.config.terminal.term = "xterm-kitty".to_string();
    runtime.config.terminal.colorterm = "24bit".to_string();

    // A pane that sets its own TERM keeps it; COLORTERM it left unset is filled
    // from the config.
    let mut base_environment_variables = BTreeMap::new();
    base_environment_variables.insert("TERM".to_string(), "screen-256color".to_string());
    let environment_variables =
        runtime.apply_terminal_identity_environment_variables(base_environment_variables);
    assert_eq!(
        environment_variables.get("TERM").map(String::as_str),
        Some("screen-256color")
    );
    assert_eq!(
        environment_variables.get("COLORTERM").map(String::as_str),
        Some("24bit")
    );
}

#[test]
fn pane_sizing_floors_the_minimum_and_carries_the_gap() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();

    // The default config gives the hard floor and no gap.
    assert_eq!(
        runtime.get_pane_sizing(),
        PaneSizing {
            minimum_size: Size {
                column_count: 2,
                row_count: 1
            },
            gap_cell_count: 0,
        }
    );

    // A configured minimum below the hard floor is raised to it, so a pane can
    // never be driven below the size a PTY can run at.
    runtime.config.pane.minimum_column_count = 0;
    runtime.config.pane.minimum_row_count = 0;
    assert_eq!(
        runtime.get_pane_sizing().minimum_size,
        Size {
            column_count: 2,
            row_count: 1
        }
    );

    // A configured minimum above the floor is honored as written.
    runtime.config.pane.minimum_column_count = 10;
    runtime.config.pane.minimum_row_count = 5;
    assert_eq!(
        runtime.get_pane_sizing().minimum_size,
        Size {
            column_count: 10,
            row_count: 5
        }
    );

    // The configured gap is carried through as written.
    runtime.config.pane.gap_cell_count = 3;
    assert_eq!(runtime.get_pane_sizing().gap_cell_count, 3);
}

#[test]
fn the_snapshot_carries_the_gap_and_leaves_it_between_two_side_by_side_panes() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport = Size {
        column_count: 80,
        row_count: 24,
    };
    let client = runtime
        .bootstrap_local(SessionId::new(), viewport, SystemTime::now())
        .expect("bootstrap");
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client),
        Command::NewPane(build_new_pane_args()),
    ));
    runtime.config.pane.gap_cell_count = 2;

    let snap = runtime.build_snapshot(client).expect("a snapshot");

    assert_eq!(snap.session_snapshot.active_tab_snapshot.gap_cell_count, 2);
    let pane_slots = &snap.session_snapshot.active_tab_snapshot.pane_slots;
    assert_eq!(pane_slots.len(), 2);
    assert_eq!(
        pane_slots[1].outer_rect.origin.column,
        pane_slots[0].outer_rect.origin.column
            + pane_slots[0].outer_rect.cell_size.column_count
            + 2
    );
}

#[test]
fn a_second_child_exit_for_the_same_pane_is_dropped_and_the_survivor_is_untouched() {
    // A `CloseOnExit` pane exits and is removed. If a duplicate exit for the now
    // gone pane arrives — two exit notices raced into the inbox — the second finds
    // no session owning the pane and drops it: no events, no panic, and the
    // surviving sibling keeps every runtime map entry it had.
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Two split panes, each with a live child and terminal engine.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    let pane_id_a = find_other_pane_id(&runtime, session_id, root_pane_id);
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    // The pane that is neither the root nor the first split.
    let pane_id_b = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != root_pane_id && *pane_id != pane_id_a)
        .expect("a third pane exists");

    // First exit removes pane_a.
    let first_exit_events = runtime.handle_child_exit(pane_id_a, ExitStatus::ExitCode(0));
    assert!(matches!(
        first_exit_events.first(),
        Some(Event::PaneProcessExited(_))
    ));
    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(pane_id_a)
        .is_none());
    let survivor_resizes = fake_pty_backend
        .list_pane_sizes(pane_id_b)
        .expect("pane_b spawned")
        .len();

    // Second exit for the same gone pane: dropped whole.
    let duplicate_exit_events = runtime.handle_child_exit(pane_id_a, ExitStatus::ExitCode(0));
    assert!(
        duplicate_exit_events.is_empty(),
        "a duplicate exit emits nothing"
    );

    // The survivor is untouched: still present, still holding all its bookkeeping,
    // and not re-resized by the dropped duplicate.
    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(pane_id_b)
        .is_some());
    assert!(runtime.pty_handle_by_pane_id.contains_key(&pane_id_b));
    assert!(runtime.pty_size_by_pane_id.contains_key(&pane_id_b));
    assert!(runtime.terminal_engine_by_pane_id.contains_key(&pane_id_b));
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id_b)
            .expect("pane_b spawned")
            .len(),
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
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    for _ in 0..2 {
        let command_envelope = build_command_envelope(
            CommandSource::from_key_binding(client_id),
            Command::NewPane(build_new_pane_args()),
        );
        assert!(matches!(
            runtime.dispatch(command_envelope),
            CommandResult::Ok { .. }
        ));
    }
    // The two spawned panes are exactly the ones with an engine; the root has none.
    let engine_pane_ids: Vec<PaneId> = runtime.list_terminal_engines().keys().copied().collect();
    assert_eq!(engine_pane_ids.len(), 2, "two spawned panes hold engines");
    let (exited, live) = (engine_pane_ids[0], engine_pane_ids[1]);

    // The exit removes the pane and its engine.
    let _ = runtime.handle_child_exit(exited, ExitStatus::ExitCode(0));
    assert!(!runtime.list_terminal_engines().contains_key(&exited));

    // Late output for the now-engineless pane is a no-op.
    runtime.handle_pty_output(exited, b"late");
    assert!(!runtime.list_terminal_engines().contains_key(&exited));

    // The live sibling still parses its output: two printable bytes advance its
    // cursor to column 2.
    runtime.handle_pty_output(live, b"hi");
    let (row, col) = runtime
        .list_terminal_engines()
        .get(&live)
        .expect("the live pane keeps its engine")
        .get_terminal_state()
        .get_active_cursor_position();
    assert_eq!((row, col), (0, 2));
}

#[test]
fn commands_still_dispatch_while_draining() {
    // `draining` is set the moment teardown begins, but no dispatch path consults
    // it yet — the field only records that shutdown started. Pin that documented
    // state: a valid command applied while draining still mutates and reports Ok.
    let (mut runtime, _runtime_event_sender, client_id, session_id) = lock_fixture();
    runtime.is_draining = true;

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        lock_mode_of(&runtime, session_id, client_id),
        LockMode::Locked
    );
    assert!(
        runtime.is_draining(),
        "dispatch does not clear the draining flag"
    );
}

#[test]
fn a_rejected_command_leaves_state_intact_and_the_next_command_works() {
    // A rejection must not be a dead end: after one command bounces off validation
    // the runtime keeps every bit of state and accepts the next command normally.
    let (mut runtime, _runtime_event_sender, client_id, session_id) = lock_fixture();

    // Close a pane that does not exist: rejected, nothing changed.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(PaneId::new()),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
    assert_eq!(
        lock_mode_of(&runtime, session_id, client_id),
        LockMode::Normal
    );

    // The very next command lands.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        lock_mode_of(&runtime, session_id, client_id),
        LockMode::Locked
    );
}

#[test]
fn a_command_after_quit_still_dispatches() {
    // `Quit` sets the loop's exit flags but does not itself gate dispatch — the
    // loop exits by polling `quit_requested`, not by dispatch refusing commands.
    // Pin that: a command issued after Quit, before the loop notices, still runs.
    let (mut runtime, _runtime_event_sender, client_id, session_id) = lock_fixture();

    assert!(matches!(
        runtime.dispatch(build_internal_command_envelope(Command::Quit)),
        CommandResult::Ok { .. }
    ));
    assert!(runtime.is_quit_requested());

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        lock_mode_of(&runtime, session_id, client_id),
        LockMode::Locked
    );
}

// A rejected command leaves a warning in the log: state is untouched and the
// session carries on, which is exactly what a warning means. The line names the
// command and the reason, so the log says what the user tried and why it did
// not happen.
#[test]
fn a_rejected_command_writes_a_warning_naming_the_reason() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let (_guard, logs) = koshi_observability::logging::with_test_writer();

    // The client this command names is attached to no session, so validation
    // rejects it on the source before it ever resolves the tab.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(ClientId::new()),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(TabId::new()),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    let command_result = runtime.dispatch(command_envelope);

    assert!(
        matches!(command_result, CommandResult::Rejected { .. }),
        "{command_result:?}"
    );
    let log_text = logs.contents();
    assert!(log_text.contains(r#""level":"WARN""#), "{log_text}");
    assert!(
        log_text.contains(r#""message":"command rejected""#),
        "{log_text}"
    );
    assert!(
        log_text.contains(&format!(r#""command_id":"{command_id}""#)),
        "{log_text}"
    );
    assert!(
        log_text.contains(r#""reason":"source client has detached""#),
        "{log_text}"
    );
    // This rejection carries no hint, so the field is left off the line rather
    // than written as an empty string.
    assert!(!log_text.contains("help"), "{log_text}");
}

// A command that dispatch accepts leaves info lines, one per event it committed
// — the success side of the same trail, so the log shows what worked as well as
// what did not.
#[test]
fn an_applied_command_writes_one_info_line_per_event_it_committed() {
    let (mut runtime, _runtime_event_sender, client_id, _session_id) = lock_fixture();
    let (_guard, logs) = koshi_observability::logging::with_test_writer();

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    let log_text = logs.contents();
    assert_eq!(
        log_text.lines().count(),
        1,
        "expected exactly one line: {log_text}"
    );
    assert!(log_text.contains(r#""level":"INFO""#), "{log_text}");
    assert!(
        log_text.contains(r#""message":"input mode changed""#),
        "{log_text}"
    );
    assert!(log_text.contains(r#""mode":"Locked""#), "{log_text}");
}

// --- Which client a client-scoped command lands on -------------------------
//
// A command like `koshi lock` acts on one client's own view. The client it
// means is the one that issued it, while that client is still attached. When
// the issuer is gone — or the pane was spawned with no designated client and
// names none — the session's sole attached client stands in. Several attached,
// or none, has no single answer and is refused.

/// A session with one tab, one live pane, and no clients yet: the fixture the
/// acting-client rules are exercised against. Returns the runtime, the keepalive
/// sender, the session, the tab, and the pane.
fn acting_client_fixture() -> (Server, mpsc::Sender<RuntimeEvent>, SessionId, TabId, PaneId) {
    let (mut runtime, runtime_event_sender) = build_runtime();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    (runtime, runtime_event_sender, session_id, tab_id, pane_id)
}

#[test]
fn lock_from_a_pane_whose_client_detached_locks_the_sole_client() {
    let (mut runtime, _runtime_event_sender, session_id, tab_id, pane_id) = acting_client_fixture();
    let attached_client_id = ClientId::new();
    let detached_client_id = ClientId::new();
    let session = runtime.session_by_id.get_mut(&session_id).expect("session");
    attach_client(session, attached_client_id, tab_id, Some(pane_id));

    // The pane's own client is gone, but exactly one client is attached, so
    // that one is the only window `koshi lock` could mean.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(detached_client_id),
        pane_id,
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(
        command_source,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        lock_mode_of(&runtime, session_id, attached_client_id),
        LockMode::Locked
    );
}

#[test]
fn lock_from_a_clientless_pane_locks_the_sole_client() {
    let (mut runtime, _runtime_event_sender, session_id, tab_id, pane_id) = acting_client_fixture();
    let attached_client_id = ClientId::new();
    let session = runtime.session_by_id.get_mut(&session_id).expect("session");
    attach_client(session, attached_client_id, tab_id, Some(pane_id));

    // A pane spawned with no designated client names none. It reads the same
    // as a client that has gone: the sole attached client stands in.
    let command_source =
        CommandSource::from_in_session_cli(session_id, None, pane_id, PathBuf::from("/sock"));
    let command_envelope = build_command_envelope(
        command_source,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        lock_mode_of(&runtime, session_id, attached_client_id),
        LockMode::Locked
    );
}

#[test]
fn lock_from_a_detached_client_with_two_attached_is_ambiguous() {
    let (mut runtime, _runtime_event_sender, session_id, tab_id, pane_id) = acting_client_fixture();
    let first_attached_client_id = ClientId::new();
    let second_attached_client_id = ClientId::new();
    let detached_client_id = ClientId::new();
    let session = runtime.session_by_id.get_mut(&session_id).expect("session");
    attach_client(session, first_attached_client_id, tab_id, Some(pane_id));
    attach_client(session, second_attached_client_id, tab_id, Some(pane_id));

    // Two windows are attached and the issuer is not one of them, so there is
    // no single window to lock. Neither is guessed at.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(detached_client_id),
        pane_id,
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(
        command_source,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some("several clients are attached; name the target client".to_string()),
        }
    );
    assert_eq!(
        lock_mode_of(&runtime, session_id, first_attached_client_id),
        LockMode::Normal
    );
    assert_eq!(
        lock_mode_of(&runtime, session_id, second_attached_client_id),
        LockMode::Normal
    );
}

#[test]
fn lock_from_an_attached_client_ignores_the_sole_client_fallback() {
    let (mut runtime, _runtime_event_sender, session_id, tab_id, pane_id) = acting_client_fixture();
    let other_client_id = ClientId::new();
    let issuer_client_id = ClientId::new();
    let session = runtime.session_by_id.get_mut(&session_id).expect("session");
    // The issuer attaches second, so a rule that reached for whichever client
    // came first would land on `other` and fail this test.
    attach_client(session, other_client_id, tab_id, Some(pane_id));
    attach_client(session, issuer_client_id, tab_id, Some(pane_id));

    // The issuer is attached, so it is the answer outright — two clients being
    // attached is only ambiguous when the issuer is not one of them.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(issuer_client_id),
        pane_id,
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(
        command_source,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        lock_mode_of(&runtime, session_id, issuer_client_id),
        LockMode::Locked
    );
    assert_eq!(
        lock_mode_of(&runtime, session_id, other_client_id),
        LockMode::Normal
    );
}

#[test]
fn fullscreen_from_a_clientless_pane_zooms_the_sole_client() {
    let (
        mut runtime,
        _fake_pty_backend,
        _runtime_event_sender,
        session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);

    // The zoom is per-client state; with one client attached, the pane the CLI
    // was issued from fills that client's view.
    let command_source =
        CommandSource::from_in_session_cli(session_id, None, pane_id_a, PathBuf::from("/sock"));
    let command_envelope = build_command_envelope(command_source, Command::TogglePaneFullscreen);
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: pane_id_a
        }
    );
}

/// `is_client_scoped` answers `true` for exactly one command. Every other
/// variant carries a target of its own, resolved by its own resolver. The
/// count assert fails when a variant is added to [`ALL_COMMAND_KINDS`], so a
/// new command has to be classified here.
#[test]
fn client_scoped_is_exactly_toggle_mouse_select() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let cases: Vec<Command> = ALL_COMMAND_KINDS
        .iter()
        .map(|command_kind| build_command_for_kind(*command_kind, tab_id, pane_id))
        .collect();

    assert_eq!(cases.len(), 20);
    for command in &cases {
        assert_eq!(
            Server::is_client_scoped(command),
            matches!(command, Command::ToggleMouseSelect),
            "{command:?}"
        );
    }
}

/// A session with two attached clients on tabs of their own: A views `tab_a`
/// with `pane_a` focused, B views `tab_b` with `pane_b` focused. Returns the
/// runtime, the keepalive sender, the session, then A's ids and B's ids.
fn two_client_fixture() -> (
    Server,
    mpsc::Sender<RuntimeEvent>,
    SessionId,
    ClientId,
    TabId,
    PaneId,
    ClientId,
    TabId,
    PaneId,
) {
    let (mut runtime, runtime_event_sender) = build_runtime();
    let (client_id_a, client_id_b) = (ClientId::new(), ClientId::new());
    let (tab_id_a, tab_id_b) = (TabId::new(), TabId::new());
    let (pane_id_a, pane_id_b) = (PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id_a);
    register_session_tab(&mut session, tab_id_a, pane_id_a);
    register_pane_record(&mut session, pane_id_b);
    register_session_tab(&mut session, tab_id_b, pane_id_b);
    attach_client(&mut session, client_id_a, tab_id_a, Some(pane_id_a));
    attach_client(&mut session, client_id_b, tab_id_b, Some(pane_id_b));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    (
        runtime,
        runtime_event_sender,
        session_id,
        client_id_a,
        tab_id_a,
        pane_id_a,
        client_id_b,
        tab_id_b,
        pane_id_b,
    )
}

#[test]
fn an_explicit_target_client_zooms_that_client() {
    let (
        runtime,
        _runtime_event_sender,
        session_id,
        _client_a,
        _tab_id_a,
        _pane_id_a,
        client_id_b,
        tab_id_b,
        pane_id_b,
    ) = two_client_fixture();

    // The named client decides both halves: its own view flips, and the pane is
    // the one it has focused in the tab it is looking at.
    let command_source = CommandSource::from_external_cli(Some(session_id), Some(client_id_b));
    let fullscreen_target = runtime
        .resolve_fullscreen_target(&command_source, Some(&runtime.session_by_id[&session_id]))
        .ok()
        .expect("the named client is attached");

    assert_eq!(fullscreen_target.client_id, client_id_b);
    assert_eq!(fullscreen_target.tab_id, tab_id_b);
    assert_eq!(fullscreen_target.pane_id, pane_id_b);
}

#[test]
fn a_target_client_in_another_session_is_not_found() {
    let (mut runtime, _runtime_event_sender, session_id, ..) = two_client_fixture();
    let stranger_client_id = ClientId::new();
    let stranger_tab_id = TabId::new();
    let stranger_pane_id = PaneId::new();
    let mut other_session = build_bare_session(SessionId::new());
    register_pane_record(&mut other_session, stranger_pane_id);
    register_session_tab(&mut other_session, stranger_tab_id, stranger_pane_id);
    attach_client(
        &mut other_session,
        stranger_client_id,
        stranger_tab_id,
        Some(stranger_pane_id),
    );
    runtime
        .session_by_id
        .insert(other_session.session_id, other_session);

    // The named client is real, but not attached here. It is refused outright,
    // never swapped for a client of this session.
    let command_source =
        CommandSource::from_external_cli(Some(session_id), Some(stranger_client_id));
    let rejection = runtime
        .resolve_fullscreen_target(&command_source, Some(&runtime.session_by_id[&session_id]))
        .err()
        .expect("a client of another session is not a target here");

    assert_eq!(rejection.reason, RejectReason::TargetNotFound);
}

#[test]
fn no_flag_with_two_clients_is_ambiguous() {
    let (runtime, _runtime_event_sender, session_id, ..) = two_client_fixture();

    // Two clients are attached and the caller named neither, so there is no
    // single view to flip.
    let command_source = CommandSource::from_external_cli(Some(session_id), None);
    let rejection = runtime
        .resolve_fullscreen_target(&command_source, Some(&runtime.session_by_id[&session_id]))
        .err()
        .expect("two attached clients and no flag has no single answer");

    assert_eq!(rejection.reason, RejectReason::TargetAmbiguous);
}

#[test]
fn no_flag_with_one_client_takes_that_client() {
    let (mut runtime, _runtime_event_sender, session_id, tab_id, pane_id) = acting_client_fixture();
    let client_id_a = ClientId::new();
    attach_client(
        runtime.session_by_id.get_mut(&session_id).expect("session"),
        client_id_a,
        tab_id,
        Some(pane_id),
    );

    // With one client attached there is only one view the command could mean.
    let command_source = CommandSource::from_external_cli(Some(session_id), None);
    let fullscreen_target = runtime
        .resolve_fullscreen_target(&command_source, Some(&runtime.session_by_id[&session_id]))
        .ok()
        .expect("the sole attached client stands in");

    assert_eq!(fullscreen_target.client_id, client_id_a);
}

#[test]
fn no_flag_with_no_client_is_a_stale_source() {
    let (runtime, _runtime_event_sender, session_id, _tab_id, _pane_id) = acting_client_fixture();

    // Nobody is attached, so there is no view to flip at all.
    let command_source = CommandSource::from_external_cli(Some(session_id), None);
    let rejection = runtime
        .resolve_fullscreen_target(&command_source, Some(&runtime.session_by_id[&session_id]))
        .err()
        .expect("no attached client is no target");

    assert_eq!(rejection.reason, RejectReason::SourceClientStale);
}

/// Two clients share one tab and `--client <B>` names B: B's own focused pane
/// fills B's screen, and A keeps the tiled view it had.
#[test]
fn a_named_client_zooms_only_its_own_view() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let (client_id_a, client_id_b) = (ClientId::new(), ClientId::new());
    let tab_id = TabId::new();
    let (focused_pane_id, other_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, focused_pane_id);
    register_pane_record(&mut session, other_pane_id);
    register_session_tab(&mut session, tab_id, focused_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(build_horizontal_split(focused_pane_id, other_pane_id));
    attach_client(&mut session, client_id_a, tab_id, Some(focused_pane_id));
    attach_client(&mut session, client_id_b, tab_id, Some(other_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), Some(client_id_b)),
        Command::TogglePaneFullscreen,
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id, ..
        } => assert_eq!(ok_id, command_id),
        other => panic!("expected Ok, got {other:?}"),
    }

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id_b, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: other_pane_id
        }
    );
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id_a, tab_id),
        LayoutMode::Tiled
    );
}

/// Naming the same client twice flips its view back. The client that was never
/// named stays tiled through both halves.
#[test]
fn naming_the_same_client_twice_returns_that_client_to_tiled() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let (client_id_a, client_id_b) = (ClientId::new(), ClientId::new());
    let tab_id = TabId::new();
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, tab_id, first_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(build_horizontal_split(first_pane_id, second_pane_id));
    attach_client(&mut session, client_id_a, tab_id, Some(first_pane_id));
    attach_client(&mut session, client_id_b, tab_id, Some(second_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_source = CommandSource::from_external_cli(Some(session_id), Some(client_id_b));
    for _ in 0..2 {
        let command_envelope =
            build_command_envelope(command_source.clone(), Command::TogglePaneFullscreen);
        let command_id = command_envelope.command_id;
        match runtime.dispatch(command_envelope) {
            CommandResult::Ok {
                command_id: ok_id, ..
            } => assert_eq!(ok_id, command_id),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id_b, tab_id),
        LayoutMode::Tiled
    );
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id_a, tab_id),
        LayoutMode::Tiled
    );
}

#[test]
fn focus_tab_from_a_detached_client_falls_back_to_the_sole_client() {
    let (mut runtime, _runtime_event_sender, session_id, tab_id, pane_id) = acting_client_fixture();
    let attached_client_id = ClientId::new();
    let detached_client_id = ClientId::new();
    let second_tab_id = TabId::new();
    let second_pane_id = PaneId::new();
    let session = runtime.session_by_id.get_mut(&session_id).expect("session");
    attach_client(session, attached_client_id, tab_id, Some(pane_id));
    register_pane_record(session, second_pane_id);
    register_session_tab(session, second_tab_id, second_pane_id);

    // A named-but-gone client falls back exactly as a source naming none does:
    // the switch lands on the one attached window.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(detached_client_id),
        pane_id,
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(
        command_source,
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(second_tab_id),
            client_id: None,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(attached_client_id)
            .expect("client")
            .get_active_tab(),
        second_tab_id
    );
}

#[test]
fn an_explicit_client_outranks_the_sole_client_fallback() {
    let (mut runtime, _runtime_event_sender, session_id, tab_id, pane_id) = acting_client_fixture();
    let issuer_client_id = ClientId::new();
    let named_client_id = ClientId::new();
    let second_tab_id = TabId::new();
    let second_pane_id = PaneId::new();
    let session = runtime.session_by_id.get_mut(&session_id).expect("session");
    attach_client(session, issuer_client_id, tab_id, Some(pane_id));
    attach_client(session, named_client_id, tab_id, Some(pane_id));
    register_pane_record(session, second_pane_id);
    register_session_tab(session, second_tab_id, second_pane_id);

    // `--client` names the window outright: the issuing client is attached and
    // still does not win, and two attached clients are not ambiguous.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(issuer_client_id),
        pane_id,
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(
        command_source,
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(second_tab_id),
            client_id: Some(named_client_id),
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(named_client_id)
            .expect("client")
            .get_active_tab(),
        second_tab_id
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(issuer_client_id)
            .expect("client")
            .get_active_tab(),
        tab_id,
        "the issuing client's own view does not move"
    );
}

#[test]
fn an_explicit_client_that_is_not_attached_never_falls_back() {
    let (mut runtime, _runtime_event_sender, session_id, tab_id, pane_id) = acting_client_fixture();
    let attached_client_id = ClientId::new();
    let stranger_client_id = ClientId::new();
    let second_tab_id = TabId::new();
    let second_pane_id = PaneId::new();
    let session = runtime.session_by_id.get_mut(&session_id).expect("session");
    attach_client(session, attached_client_id, tab_id, Some(pane_id));
    register_pane_record(session, second_pane_id);
    register_session_tab(session, second_tab_id, second_pane_id);

    // Naming a window that is not there is an error, not an invitation to pick
    // the one that is: a command aimed at a specific client never lands on
    // another one.
    let command_source =
        CommandSource::from_in_session_cli(session_id, None, pane_id, PathBuf::from("/sock"));
    let command_envelope = build_command_envelope(
        command_source,
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(second_tab_id),
            client_id: Some(stranger_client_id),
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(attached_client_id)
            .expect("client")
            .get_active_tab(),
        tab_id
    );
}

#[test]
fn fullscreen_from_a_pane_on_a_tab_nobody_views_is_refused() {
    let (mut runtime, _runtime_event_sender, session_id, tab_id, pane_id) = acting_client_fixture();
    let attached_client_id = ClientId::new();
    let background_tab_id = TabId::new();
    let background_pane_id = PaneId::new();
    let session = runtime.session_by_id.get_mut(&session_id).expect("session");
    attach_client(session, attached_client_id, tab_id, Some(pane_id));
    register_pane_record(session, background_pane_id);
    register_session_tab(session, background_tab_id, background_pane_id);

    // The fallback client is a real client, but it is looking at another tab.
    // Zooming changes what a client draws, and nobody draws this pane's tab, so
    // there is no view to change and nothing is mutated.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        None,
        background_pane_id,
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(command_source, Command::TogglePaneFullscreen);
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane's tab is not viewed by any client".to_string()),
        }
    );
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, attached_client_id, background_tab_id,),
        LayoutMode::Tiled
    );
}

#[test]
fn lock_from_a_pane_on_a_background_tab_still_locks_the_sole_client() {
    let (mut runtime, _runtime_event_sender, session_id, tab_id, pane_id) = acting_client_fixture();
    let attached_client_id = ClientId::new();
    let background_tab_id = TabId::new();
    let background_pane_id = PaneId::new();
    let session = runtime.session_by_id.get_mut(&session_id).expect("session");
    attach_client(session, attached_client_id, tab_id, Some(pane_id));
    register_pane_record(session, background_pane_id);
    register_session_tab(session, background_tab_id, background_pane_id);

    // Lock mode is the client's own state with no pane or tab in it, so which
    // tab the issuing pane sits on does not matter.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        None,
        background_pane_id,
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(
        command_source,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        lock_mode_of(&runtime, session_id, attached_client_id),
        LockMode::Locked
    );
}

// --- External targeting: acting-client defaults, tab-anchored new-pane,
// --- explicit lock client ---

#[test]
fn external_pane_default_acts_on_the_sole_clients_focused_pane() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        session_id,
        _client_id,
        _root_pane_id,
        pane_id_a,
        pane_pty_size_a,
    ) = build_resize_fixture();

    // No --pane: the external command acts where a keypress on the sole
    // attached client would — its focused pane, `pane_a` (the fresh split).
    // Growing its left border by 5 takes 5 columns from root.
    let command_source = CommandSource::from_external_cli(Some(session_id), None);
    let command_envelope = build_command_envelope(
        command_source,
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Left,
            resize_amount_cells: 5,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    // The resize landed on `pane_a` alone: its PTY grew by the 5 columns.
    let expected_pty_size = PtySize {
        column_count: pane_pty_size_a.column_count + 5,
        row_count: pane_pty_size_a.row_count,
    };
    assert_eq!(runtime.pty_size_by_pane_id[&pane_id_a], expected_pty_size);
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(pane_id_a)
            .unwrap()
            .last()
            .unwrap(),
        expected_pty_size
    );
}

#[test]
fn external_pane_default_with_two_clients_is_ambiguous() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, ClientId::new(), tab_id, Some(root_pane_id));
    attach_client(&mut session, ClientId::new(), tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Two clients could each mean a different focused pane; never guess.
    let command_source = CommandSource::from_external_cli(Some(session_id), None);
    let command_envelope = build_command_envelope(
        command_source,
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Left,
            resize_amount_cells: 1,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some("several clients are attached; name the target client".to_string()),
        }
    );
}

#[test]
fn external_tab_default_acts_on_the_sole_clients_active_tab() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let front_tab_id = TabId::new();
    let front_pane_id = PaneId::new();
    let back_tab_id = TabId::new();
    let back_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_pane_id);
    register_pane_record(&mut session, back_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_pane_id);
    attach_client(&mut session, client_id, front_tab_id, Some(front_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // No --tab: the sole attached client's active tab is the target.
    let command_source = CommandSource::from_external_cli(Some(session_id), None);
    let command_envelope = build_command_envelope(
        command_source,
        Command::MoveTab(MoveTabArgs {
            tab_id: None,
            target_tab_index: 1,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    // The move landed on the client's active tab alone: it took slot 1 and
    // the background tab slid to the front.
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs[&front_tab_id].get_tab_index(), 1);
    assert_eq!(session.tabs[&back_tab_id].get_tab_index(), 0);
}

#[test]
fn new_pane_with_a_tab_target_splits_that_tabs_recent_pane() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let front_tab_id = TabId::new();
    let front_pane_id = PaneId::new();
    let back_tab_id = TabId::new();
    let back_first_pane_id = PaneId::new();
    let back_second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_pane_id);
    register_pane_record(&mut session, back_first_pane_id);
    register_pane_record(&mut session, back_second_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_first_pane_id);
    let split_layout = split_leaf(
        session.tabs[&back_tab_id].get_layout_tree(),
        back_first_pane_id,
        back_second_pane_id,
        Direction::Right,
    )
    .expect("back_first_pane_id is a leaf");
    session
        .tabs
        .get_mut(&back_tab_id)
        .expect("tab")
        .update_layout(split_layout);
    // `back_second_pane_id` was focused most recently, so it is the split anchor.
    session
        .tabs
        .get_mut(&back_tab_id)
        .expect("tab")
        .record_focus_mru(back_second_pane_id);
    attach_client(&mut session, client_id, front_tab_id, Some(front_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: None,
            tab_id: Some(back_tab_id),
            direction: Direction::Right,
            should_stack: false,
            working_directory: None,
            spawn_spec: None,
            client_id: None,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    // The new pane split `back_second_pane_id` (the tab's most recently focused
    // pane), so layout order reads: first, second, new.
    let leaf_pane_ids = runtime.session_by_id[&session_id].tabs[&back_tab_id]
        .get_layout_tree()
        .list_leaf_pane_ids();
    assert_eq!(leaf_pane_ids.len(), 3);
    assert_eq!(leaf_pane_ids[0], back_first_pane_id);
    assert_eq!(leaf_pane_ids[1], back_second_pane_id);
    let new_pane_id = leaf_pane_ids[2];
    // The issuing client was switched onto the target tab and focuses the
    // new pane.
    let client = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("client");
    assert_eq!(client.get_active_tab(), back_tab_id);
    assert_eq!(client.get_focused_pane(back_tab_id), Some(new_pane_id));
}

#[test]
fn new_pane_tab_target_with_no_focus_history_splits_the_first_pane() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let front_tab_id = TabId::new();
    let front_pane_id = PaneId::new();
    let back_tab_id = TabId::new();
    let back_first_pane_id = PaneId::new();
    let back_second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, front_pane_id);
    register_pane_record(&mut session, back_first_pane_id);
    register_pane_record(&mut session, back_second_pane_id);
    register_session_tab(&mut session, front_tab_id, front_pane_id);
    register_session_tab(&mut session, back_tab_id, back_first_pane_id);
    let split_layout = split_leaf(
        session.tabs[&back_tab_id].get_layout_tree(),
        back_first_pane_id,
        back_second_pane_id,
        Direction::Right,
    )
    .expect("back_first_pane_id is a leaf");
    session
        .tabs
        .get_mut(&back_tab_id)
        .expect("tab")
        .update_layout(split_layout);
    attach_client(&mut session, client_id, front_tab_id, Some(front_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: None,
            tab_id: Some(back_tab_id),
            direction: Direction::Right,
            should_stack: false,
            working_directory: None,
            spawn_spec: None,
            client_id: None,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    // Nothing in the tab was ever focused, so the anchor falls back to the
    // first pane in layout order: the new pane splits `back_first_pane_id`.
    let leaf_pane_ids = runtime.session_by_id[&session_id].tabs[&back_tab_id]
        .get_layout_tree()
        .list_leaf_pane_ids();
    assert_eq!(leaf_pane_ids.len(), 3);
    assert_eq!(leaf_pane_ids[0], back_first_pane_id);
    assert_eq!(leaf_pane_ids[2], back_second_pane_id);
}

#[test]
fn new_pane_with_an_unknown_tab_target_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: None,
            tab_id: Some(TabId::new()),
            direction: Direction::Right,
            should_stack: false,
            working_directory: None,
            spawn_spec: None,
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: None,
        }
    );
}

#[test]
fn lock_with_an_explicit_client_locks_that_client() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let issuer_client_id = ClientId::new();
    let target_client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, issuer_client_id, tab_id, Some(root_pane_id));
    attach_client(&mut session, target_client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // --client outranks the issuer: the named client locks, the issuer does
    // not.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(issuer_client_id),
        Command::SetLockMode(LockModeArgs {
            is_locked: true,
            client_id: Some(target_client_id),
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        lock_mode_of(&runtime, session_id, target_client_id),
        LockMode::Locked
    );
    assert_eq!(
        lock_mode_of(&runtime, session_id, issuer_client_id),
        LockMode::Normal
    );
}

#[test]
fn toggle_lock_with_an_explicit_client_flips_that_client() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let issuer_client_id = ClientId::new();
    let target_client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, issuer_client_id, tab_id, Some(root_pane_id));
    attach_client(&mut session, target_client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(issuer_client_id),
        Command::ToggleLockMode(ToggleLockModeArgs {
            client_id: Some(target_client_id),
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        lock_mode_of(&runtime, session_id, target_client_id),
        LockMode::Locked
    );
    assert_eq!(
        lock_mode_of(&runtime, session_id, issuer_client_id),
        LockMode::Normal
    );
}

#[test]
fn lock_with_an_unattached_explicit_client_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let issuer_client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, issuer_client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // An explicit target that is not attached refuses outright; it never
    // falls back to the issuer.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(issuer_client_id),
        Command::SetLockMode(LockModeArgs {
            is_locked: true,
            client_id: Some(ClientId::new()),
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    assert_eq!(
        lock_mode_of(&runtime, session_id, issuer_client_id),
        LockMode::Normal
    );
}

#[test]
fn external_lock_defaults_to_the_sole_attached_client() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // `koshi lock` from outside: the sole attached client is the target.
    let command_source = CommandSource::from_external_cli(Some(session_id), None);
    let command_envelope = build_command_envelope(
        command_source,
        Command::SetLockMode(LockModeArgs {
            is_locked: true,
            client_id: None,
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));
    assert_eq!(
        lock_mode_of(&runtime, session_id, client_id),
        LockMode::Locked
    );
}

// --- Working-directory inheritance for new panes and tabs -------------------

/// The working directory used for the last spawned pane.
fn get_last_spawn_working_directory(fake_pty_backend: &FakePtyBackend) -> Option<PathBuf> {
    let pane_id = *fake_pty_backend
        .list_spawned_pane_ids()
        .last()
        .expect("a spawned pane");
    fake_pty_backend
        .get_spawn_spec(pane_id)
        .expect("spawn spec")
        .working_directory
}

#[test]
fn new_pane_with_no_working_directory_opens_in_the_source_panes_reported_directory() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();
    // The shell in the focused pane reports its directory over OSC 7.
    runtime.handle_pty_output(pane_id_a, b"\x1b]7;file:///tmp/reported\x07");

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        get_last_spawn_working_directory(&fake_pty_backend),
        Some(PathBuf::from("/tmp/reported"))
    );
}

#[test]
fn the_shells_report_wins_over_the_os_answer() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();
    runtime.handle_pty_output(pane_id_a, b"\x1b]7;file:///tmp/reported\x07");
    fake_pty_backend.set_live_working_directory(pane_id_a, "/tmp/os-answer");

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        get_last_spawn_working_directory(&fake_pty_backend),
        Some(PathBuf::from("/tmp/reported"))
    );
}

#[test]
fn a_remote_shells_reported_directory_is_not_inherited() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();
    // A shell over SSH reports a directory on another machine; the OS's
    // answer for the local child (the ssh process) is used instead.
    runtime.handle_pty_output(pane_id_a, b"\x1b]7;file://build-server/srv/remote\x07");
    fake_pty_backend.set_live_working_directory(pane_id_a, "/tmp/os-answer");

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        get_last_spawn_working_directory(&fake_pty_backend),
        Some(PathBuf::from("/tmp/os-answer"))
    );
}

#[test]
fn new_pane_with_no_working_directory_falls_back_to_the_os_answer() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();
    // No OSC 7 report; the OS knows where the child currently is.
    fake_pty_backend.set_live_working_directory(pane_id_a, "/tmp/os-answer");

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        get_last_spawn_working_directory(&fake_pty_backend),
        Some(PathBuf::from("/tmp/os-answer"))
    );
}

#[test]
fn new_pane_with_no_working_directory_falls_back_to_the_source_panes_spawn_directory() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        _pane_id_a,
        _size_a,
    ) = build_resize_fixture();
    // Split a pane into a known directory; it takes focus.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            working_directory: Some(PathBuf::from("/srv/spawned")),
            ..build_new_pane_args()
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    // No OSC 7 report and no OS answer: the split inherits the working directory
    // the focused pane was spawned in.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        get_last_spawn_working_directory(&fake_pty_backend),
        Some(PathBuf::from("/srv/spawned"))
    );
}

#[test]
fn an_explicit_working_directory_wins_over_the_source_panes_directory() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();
    runtime.handle_pty_output(pane_id_a, b"\x1b]7;file:///tmp/reported\x07");

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            working_directory: Some(PathBuf::from("/explicit")),
            ..build_new_pane_args()
        }),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        get_last_spawn_working_directory(&fake_pty_backend),
        Some(PathBuf::from("/explicit"))
    );
}

#[test]
fn a_loopback_reported_host_counts_as_this_machine() {
    // The URI form brackets an IPv6 literal; sloppy shell hooks write it
    // bare. All four spellings mean the local machine; a real remote name
    // does not.
    for local_host in [
        None,
        Some("localhost"),
        Some("LOCALHOST"),
        Some("127.0.0.1"),
        Some("127.0.0.2"),
        Some("[127.0.0.1]"),
        Some("::1"),
        Some("[::1]"),
        Some("0:0:0:0:0:0:0:1"),
    ] {
        assert!(
            is_local_host(local_host),
            "{local_host:?} must count as local"
        );
    }
    assert!(!is_local_host(Some("build-server")));
    assert!(!is_local_host(Some("10.0.0.1")));
}

#[test]
fn new_tab_with_no_working_directory_opens_in_the_focused_panes_directory() {
    let (
        mut runtime,
        fake_pty_backend,
        _runtime_event_sender,
        _session_id,
        client_id,
        _root_pane_id,
        pane_id_a,
        _size_a,
    ) = build_resize_fixture();
    runtime.handle_pty_output(pane_id_a, b"\x1b]7;file:///tmp/reported\x07");

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    assert_eq!(
        get_last_spawn_working_directory(&fake_pty_backend),
        Some(PathBuf::from("/tmp/reported"))
    );
}

#[test]
fn detaching_the_last_client_leaves_the_session_running_with_no_clients() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);
    let events = runtime.subscribe(client, EventFilter::All);

    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::Detach(DetachArgs {
            client_id: Some(client),
        }),
    );
    let command_id = command_envelope.command_id;

    // The tab loses its last viewer, so it has no viewport to reflow to and the
    // detach emits nothing.
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
    assert_eq!(runtime.session_by_id[&session_id].clients.client_count(), 0);
    assert_eq!(runtime.event_bus.subscriber_count(), 0);

    // The session outlives its last client: the pane is still registered and
    // still holds its PTY.
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(pane_id)
            .map(PaneRecord::get_pane_id),
        Some(pane_id)
    );
    assert!(runtime.pty_handle_by_pane_id.contains_key(&pane_id));
    drop(events);
}

#[test]
fn detach_all_takes_every_attached_client() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let initial_client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, pane_id) = get_only_session_slot(&runtime);
    let additional_client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        additional_client_id,
        viewport_size,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );
    let initial_client_events = runtime.subscribe(initial_client_id, EventFilter::All);
    let additional_client_events = runtime.subscribe(additional_client_id, EventFilter::All);

    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::DetachAll,
    );
    let command_id = command_envelope.command_id;

    // Both clients view the tab at the same size, so neither departure changes
    // it and nothing reflows.
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );

    assert_eq!(runtime.session_by_id[&session_id].clients.client_count(), 0);
    assert!(runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(initial_client_id)
        .is_none());
    assert!(runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(additional_client_id)
        .is_none());

    // Both subscriptions go with the records, and the session's pane lives on.
    assert_eq!(runtime.event_bus.subscriber_count(), 0);
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(pane_id)
            .map(PaneRecord::get_pane_id),
        Some(pane_id)
    );
    drop(initial_client_events);
    drop(additional_client_events);
}

#[test]
fn detach_naming_a_client_that_is_not_attached_is_not_found() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let attached_client_id = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, _pane_id) = get_only_session_slot(&runtime);

    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::Detach(DetachArgs {
            client_id: Some(ClientId::new()),
        }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(attached_client_id)
            .map(Client::get_client_id),
        Some(attached_client_id)
    );
}

#[test]
fn detach_with_several_attached_and_none_named_lists_the_ids_to_choose_from() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let initial_client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);
    let additional_client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        additional_client_id,
        viewport_size,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );

    // An external CLI names no client of its own, and two are attached.
    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::Detach(DetachArgs { client_id: None }),
    );
    let command_id = command_envelope.command_id;

    // The registry lists clients in id order, which for these ids is the order
    // their text sorts in.
    let mut client_id_strings = [
        initial_client_id.to_string(),
        additional_client_id.to_string(),
    ];
    client_id_strings.sort();
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some(format!(
                "several clients are attached; specify the client: {}, {}",
                client_id_strings[0], client_id_strings[1]
            )),
        }
    );
    assert_eq!(runtime.session_by_id[&session_id].clients.client_count(), 2);
}

#[test]
fn detach_with_a_sole_attached_client_and_none_named_takes_that_client() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let only_client_id = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);
    let events = runtime.subscribe(only_client_id, EventFilter::All);

    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::Detach(DetachArgs { client_id: None }),
    );
    let command_id = command_envelope.command_id;

    // The tab loses its last viewer, so it has no viewport to reflow to and
    // the detach emits nothing.
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
    assert_eq!(runtime.session_by_id[&session_id].clients.client_count(), 0);
    assert!(runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(only_client_id)
        .is_none());
    assert_eq!(runtime.event_bus.subscriber_count(), 0);

    // The session model lives on: the pane is still registered and still holds
    // its PTY.
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(pane_id)
            .map(PaneRecord::get_pane_id),
        Some(pane_id)
    );
    assert!(runtime.pty_handle_by_pane_id.contains_key(&pane_id));
    drop(events);
}

#[test]
fn detach_all_on_a_session_with_no_clients_applies_and_emits_nothing() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let mut session = build_bare_session(SessionId::new());
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::DetachAll,
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
    assert_eq!(runtime.session_by_id[&session_id].clients.client_count(), 0);
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(pane_id)
            .map(PaneRecord::get_pane_id),
        Some(pane_id)
    );
}

#[test]
fn detach_all_only_detaches_clients_of_the_acting_session() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let session_id_a = SessionId::new();
    let session_id_b = SessionId::new();
    let client_id_a = ClientId::new();
    let client_id_b = ClientId::new();

    for (session_id, client_id) in [(session_id_a, client_id_a), (session_id_b, client_id_b)] {
        let mut session = build_bare_session(session_id);
        let pane_id = PaneId::new();
        let tab_id = TabId::new();
        register_pane_record(&mut session, pane_id);
        register_session_tab(&mut session, tab_id, pane_id);
        attach_client(&mut session, client_id, tab_id, None);
        runtime.session_by_id.insert(session_id, session);
    }

    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id_a), None),
        Command::DetachAll,
    );
    let command_id = command_envelope.command_id;

    let CommandResult::Ok {
        command_id: done_id,
        ..
    } = runtime.dispatch(command_envelope)
    else {
        panic!("detach-all on the named session dispatches Ok");
    };
    assert_eq!(done_id, command_id);

    // The named session drained; the other session's client is untouched.
    assert_eq!(
        runtime.session_by_id[&session_id_a].clients.client_count(),
        0
    );
    assert_eq!(
        runtime.session_by_id[&session_id_b]
            .clients
            .get_client_by_id(client_id_b)
            .map(Client::get_client_id),
        Some(client_id_b)
    );
}

#[test]
fn a_switch_puts_the_session_to_join_on_the_clients_queue() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);
    let events = runtime.subscribe(client, EventFilter::All);
    let target_session_id = SessionId::new();

    let command_envelope = build_command_envelope(
        CommandSource::from_in_session_cli(
            session_id,
            Some(client),
            pane_id,
            PathBuf::from("/sock"),
        ),
        Command::SwitchSession(SwitchSessionArgs {
            client_id: None,
            session_id: target_session_id,
        }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );

    let session_switch_targets: Vec<SessionId> = events
        .try_iter()
        .filter_map(|delivery| match delivery {
            Delivery::SwitchTo(session_id) => Some(session_id),
            _ => None,
        })
        .collect();
    assert_eq!(session_switch_targets, vec![target_session_id]);
}

#[test]
fn a_switch_into_the_session_the_client_is_already_in_is_refused() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);
    let events = runtime.subscribe(client, EventFilter::All);

    let command_envelope = build_command_envelope(
        CommandSource::from_in_session_cli(
            session_id,
            Some(client),
            pane_id,
            PathBuf::from("/sock"),
        ),
        Command::SwitchSession(SwitchSessionArgs {
            client_id: None,
            session_id,
        }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("this client is already in that session".to_string()),
        }
    );
    assert!(
        !events
            .try_iter()
            .any(|delivery| matches!(delivery, Delivery::SwitchTo(_))),
        "a refused switch queues no move"
    );
}

/// A plugin source resolves no session, so validation refuses the switch
/// before the handler's own `session_switch` check runs.
#[test]
fn a_switch_is_refused_for_a_plugin() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let _ = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");

    let command_envelope = build_command_envelope(
        CommandSource::from_plugin(PluginId::new()),
        Command::SwitchSession(SwitchSessionArgs {
            client_id: None,
            session_id: SessionId::new(),
        }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("no session context".to_string()),
        }
    );
}

#[test]
fn a_switch_naming_a_client_that_is_not_attached_is_refused() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);
    let stranger_client_id = ClientId::new();

    let command_envelope = build_command_envelope(
        CommandSource::from_in_session_cli(
            session_id,
            Some(client),
            pane_id,
            PathBuf::from("/sock"),
        ),
        Command::SwitchSession(SwitchSessionArgs {
            client_id: Some(stranger_client_id),
            session_id: SessionId::new(),
        }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );
}

#[test]
fn a_client_that_switched_away_ends_the_session_under_auto_close() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);
    runtime.config.should_auto_close_session = true;
    // An attach registers the client and its subscriber together, so a client
    // that can be moved always has one.
    let _events = runtime.subscribe(client, EventFilter::All);

    let command_envelope = build_command_envelope(
        CommandSource::from_in_session_cli(
            session_id,
            Some(client),
            pane_id,
            PathBuf::from("/sock"),
        ),
        Command::SwitchSession(SwitchSessionArgs {
            client_id: None,
            session_id: SessionId::new(),
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
    // Queueing the move leaves the client attached; the session ends only once
    // that client's connection goes, which is an ordinary detach.
    assert!(!runtime.is_quit_requested());

    runtime.handle_client_detach(client);

    assert!(runtime.is_quit_requested());
}

/// The switch honours an explicitly named client the way a detach does. Both
/// resolve through the same helper, in validation and again in the handler, so
/// naming a client is enough even when the source names none of its own.
#[test]
fn a_switch_moves_the_client_it_names_with_several_attached() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let initial_client_id = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);
    let additional_client_id = ClientId::new();
    attach_client(
        runtime
            .session_by_id
            .get_mut(&session_id)
            .expect("the session"),
        additional_client_id,
        tab_id,
        None,
    );
    let session_switch_targets = runtime.subscribe(initial_client_id, EventFilter::All);
    let target_session_id = SessionId::new();

    // An external source names no client of its own, so only `client` says who
    // moves.
    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::SwitchSession(SwitchSessionArgs {
            client_id: Some(initial_client_id),
            session_id: target_session_id,
        }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
    assert_eq!(
        session_switch_targets
            .try_iter()
            .filter_map(|delivery| match delivery {
                Delivery::SwitchTo(session_id) => Some(session_id),
                _ => None,
            })
            .collect::<Vec<SessionId>>(),
        vec![target_session_id]
    );
}

/// A client so far behind that its queue is full cannot be handed the move, and
/// the move is never replayed, so the switch is refused rather than reported as
/// done.
#[test]
fn a_switch_is_refused_when_the_clients_queue_is_full() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, pane_id) = get_only_session_slot(&runtime);
    let _events = runtime.subscribe(client, EventFilter::All);

    // Nothing reads the queue, so publishing its whole capacity fills it.
    let backlog: Vec<Event> = (0..crate::runtime::bus::SUBSCRIBER_QUEUE_CAPACITY)
        .map(|_| Event::TabCreated(koshi_core::event::TabCreated { tab_id }))
        .collect();
    runtime.publish_events(&backlog);

    let command_envelope = build_command_envelope(
        CommandSource::from_in_session_cli(
            session_id,
            Some(client),
            pane_id,
            PathBuf::from("/sock"),
        ),
        Command::SwitchSession(SwitchSessionArgs {
            client_id: None,
            session_id: SessionId::new(),
        }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("the client is too far behind to be moved right now; try again".to_string()),
        }
    );
}

#[test]
fn a_client_the_router_bridged_from_another_machine_is_recorded_remote() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), viewport, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);

    let joining_client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        joining_client_id,
        viewport,
        None,
        tab_id,
        SystemTime::now(),
        true,
    );

    assert_eq!(
        runtime
            .session_by_id
            .get(&session_id)
            .expect("session")
            .clients
            .get_client_by_id(joining_client_id)
            .expect("the attached client")
            .get_origin(),
        ClientOrigin::Remote
    );
}

#[test]
fn a_client_that_reached_this_machine_directly_is_recorded_local() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), viewport, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);

    let joining_client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        joining_client_id,
        viewport,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );

    assert_eq!(
        runtime
            .session_by_id
            .get(&session_id)
            .expect("session")
            .clients
            .get_client_by_id(joining_client_id)
            .expect("the attached client")
            .get_origin(),
        ClientOrigin::Local
    );
}

#[test]
fn resuming_a_local_clients_id_over_a_remote_connection_records_it_remote() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), viewport, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);

    let client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        client_id,
        viewport,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );
    assert_eq!(
        runtime
            .session_by_id
            .get(&session_id)
            .expect("session")
            .clients
            .get_client_by_id(client_id)
            .expect("the attached client")
            .get_origin(),
        ClientOrigin::Local
    );

    runtime.handle_client_attach(
        session_id,
        client_id,
        viewport,
        None,
        tab_id,
        SystemTime::now(),
        true,
    );

    assert_eq!(
        runtime
            .session_by_id
            .get(&session_id)
            .expect("session")
            .clients
            .get_client_by_id(client_id)
            .expect("the re-attached client")
            .get_origin(),
        ClientOrigin::Remote,
        "re-attaching the same id over a remote connection left it recorded local"
    );
}

#[test]
fn a_remote_client_that_comes_back_on_a_local_connection_records_it_local() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), viewport, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);

    let client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        client_id,
        viewport,
        None,
        tab_id,
        SystemTime::now(),
        true,
    );

    runtime.handle_client_attach(
        session_id,
        client_id,
        viewport,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );

    assert_eq!(
        runtime
            .session_by_id
            .get(&session_id)
            .expect("session")
            .clients
            .get_client_by_id(client_id)
            .expect("the re-attached client")
            .get_origin(),
        ClientOrigin::Local
    );
}

/// A client with no room to draw a pane gives the session no size to build a
/// tab against: the new tab is refused.
#[test]
fn a_new_tab_for_a_starving_client_is_rejected_for_size() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client_with_reported_pane_area(
        &mut session,
        client_id,
        tab_id,
        Some(pane_id),
        Some(PaneArea::Starving),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("not enough space for a new tab".to_string()),
        }
    );
    assert_eq!(runtime.session_by_id[&session_id].tabs.len(), 1);
    assert_eq!(
        fake_pty_backend.list_spawned_pane_ids(),
        Vec::<PaneId>::new()
    );
}

/// The tab's only viewer is starving, so neither the tab nor the issuer offers
/// a size for the split to fit into.
#[test]
fn a_new_pane_for_a_starving_client_with_no_other_viewer_is_rejected_for_size() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client_with_reported_pane_area(
        &mut session,
        client_id,
        tab_id,
        Some(pane_id),
        Some(PaneArea::Starving),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinSize,
            help: Some("not enough space for a new pane".to_string()),
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id].panes.pane_record_count(),
        1
    );
    assert_eq!(
        fake_pty_backend.list_spawned_pane_ids(),
        Vec::<PaneId>::new()
    );
}

/// A second viewer sizes the tab at 80x22; the starving issuer's split fits
/// that size.
#[test]
fn a_new_pane_for_a_starving_client_uses_the_other_viewers_size() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let starving_client_id = ClientId::new();
    let sizing_client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    attach_client_with_reported_pane_area(
        &mut session,
        starving_client_id,
        tab_id,
        Some(pane_id),
        Some(PaneArea::Starving),
    );
    attach_client(&mut session, sizing_client_id, tab_id, Some(pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(starving_client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert!(matches!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok { .. }
    ));

    let new_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|candidate_pane_id| *candidate_pane_id != pane_id)
        .expect("the split pane");

    // The sizing client's 80x24 terminal minus its two chrome rows.
    let solved_pane_sizes = compute_pane_spawn_sizes(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        Size {
            column_count: 80,
            row_count: 22,
        },
        PaneSizing::default(),
    );
    let expected_pty_size = solved_pane_sizes
        .iter()
        .find(|(pane_id, _)| *pane_id == new_pane_id)
        .map(|(_, pane_size)| *pane_size)
        .expect("the split pane is solved");
    assert_eq!(runtime.pty_size_by_pane_id[&new_pane_id], expected_pty_size);
}

/// A resize that reports a pane area smaller than the terminal reflows every
/// live pane of the tab to the solve of that area, once each.
#[test]
fn a_resize_reporting_a_smaller_pane_area_resizes_each_pane_once() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport = Size {
        column_count: 120,
        row_count: 40,
    };
    let client = runtime
        .bootstrap_local(SessionId::new(), viewport, SystemTime::now())
        .expect("bootstrap");
    let (session_id, tab_id, first_pane_id) = get_only_session_slot(&runtime);
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client),
        Command::NewPane(build_new_pane_args()),
    ));
    let second_pane_id = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| *pane_id != first_pane_id)
        .expect("the split pane");

    let reported_pane_size = Size {
        column_count: 60,
        row_count: 20,
    };
    let events = runtime.handle_client_resize(
        client,
        viewport,
        Some(PaneArea::Reported(reported_pane_size)),
    );

    let solved_pane_sizes = compute_pane_spawn_sizes(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        reported_pane_size,
        PaneSizing::default(),
    );
    let get_solved_pane_size = |wanted_pane_id: PaneId| {
        solved_pane_sizes
            .iter()
            .find(|(pane_id, _)| *pane_id == wanted_pane_id)
            .map(|(_, size)| *size)
            .expect("the pane is solved")
    };
    assert_eq!(
        events,
        vec![
            Event::PtyResized(PtyResized {
                pane_id: first_pane_id,
                pty_size: get_solved_pane_size(first_pane_id),
            }),
            Event::PtyResized(PtyResized {
                pane_id: second_pane_id,
                pty_size: get_solved_pane_size(second_pane_id),
            }),
        ]
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(first_pane_id)
            .expect("resizes")
            .last()
            .unwrap(),
        get_solved_pane_size(first_pane_id)
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(second_pane_id)
            .expect("resizes")
            .last()
            .unwrap(),
        get_solved_pane_size(second_pane_id)
    );
}

/// A reported pane area of `0x0` gives the tab an effective size of `0x0`:
/// every pane is suppressed, so no PTY is resized and every PTY keeps its
/// size.
#[test]
fn a_resize_reporting_a_zero_pane_area_resizes_no_pty() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport = Size {
        column_count: 120,
        row_count: 40,
    };
    let client = runtime
        .bootstrap_local(SessionId::new(), viewport, SystemTime::now())
        .expect("bootstrap");
    let (session_id, _tab_id, first_pane_id) = get_only_session_slot(&runtime);
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client),
        Command::NewPane(build_new_pane_args()),
    ));
    let second_pane_id = find_other_pane_id(&runtime, session_id, first_pane_id);
    let pty_sizes_before = runtime.pty_size_by_pane_id.clone();
    let first_pane_size_history = fake_pty_backend
        .list_pane_sizes(first_pane_id)
        .expect("resizes");
    let second_pane_size_history = fake_pty_backend
        .list_pane_sizes(second_pane_id)
        .expect("resizes");

    let events = runtime.handle_client_resize(
        client,
        viewport,
        Some(PaneArea::Reported(Size {
            column_count: 0,
            row_count: 0,
        })),
    );

    assert_eq!(events, Vec::new());
    assert_eq!(runtime.pty_size_by_pane_id, pty_sizes_before);
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(first_pane_id)
            .expect("resizes"),
        first_pane_size_history
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(second_pane_id)
            .expect("resizes"),
        second_pane_size_history
    );
    let snapshot = runtime.build_snapshot(client).expect("a frame");
    assert!(
        snapshot
            .session_snapshot
            .active_tab_snapshot
            .are_all_panes_suppressed
    );
}

/// A client that reported starving and then reports a size gets the tab
/// resized to that size, one resize per pane.
#[test]
fn a_client_reporting_a_size_after_starving_resizes_each_pane_again() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport = Size {
        column_count: 120,
        row_count: 40,
    };
    let client = runtime
        .bootstrap_local(SessionId::new(), viewport, SystemTime::now())
        .expect("bootstrap");
    let (session_id, tab_id, first_pane_id) = get_only_session_slot(&runtime);
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client),
        Command::NewPane(build_new_pane_args()),
    ));
    let second_pane_id = find_other_pane_id(&runtime, session_id, first_pane_id);
    assert_eq!(
        runtime.handle_client_resize(client, viewport, Some(PaneArea::Starving)),
        Vec::new()
    );

    let reported_pane_size = Size {
        column_count: 60,
        row_count: 20,
    };
    let events = runtime.handle_client_resize(
        client,
        viewport,
        Some(PaneArea::Reported(reported_pane_size)),
    );

    let solved_pane_sizes = compute_pane_spawn_sizes(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        reported_pane_size,
        PaneSizing::default(),
    );
    let get_solved_pane_size = |wanted_pane_id: PaneId| {
        solved_pane_sizes
            .iter()
            .find(|(pane_id, _)| *pane_id == wanted_pane_id)
            .map(|(_, size)| *size)
            .expect("the pane is solved")
    };
    assert_eq!(
        events,
        vec![
            Event::PtyResized(PtyResized {
                pane_id: first_pane_id,
                pty_size: get_solved_pane_size(first_pane_id),
            }),
            Event::PtyResized(PtyResized {
                pane_id: second_pane_id,
                pty_size: get_solved_pane_size(second_pane_id),
            }),
        ]
    );
}

/// Closing the focused pane while its only viewer is starving still repairs
/// that viewer's focus onto the survivor.
#[test]
fn close_pane_for_a_starving_sole_viewer_refocuses_the_survivor() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, left_pane_id);
    register_pane_record(&mut session, right_pane_id);
    register_session_tab(&mut session, tab_id, left_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .unwrap()
        .update_layout(build_horizontal_split(left_pane_id, right_pane_id));
    attach_client_with_reported_pane_area(
        &mut session,
        client_id,
        tab_id,
        Some(left_pane_id),
        Some(PaneArea::Starving),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_id,
            emitted_events,
        } => {
            assert_eq!(ok_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(right_pane_id)
    );
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(right_pane_id)
    );
}

/// The tab's only viewer reports it has no room to draw, so the tab has no
/// effective size and no PTY moves.
#[test]
fn a_resize_reporting_starving_leaves_the_tab_sizes_unchanged() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport = Size {
        column_count: 80,
        row_count: 24,
    };
    let client = runtime
        .bootstrap_local(SessionId::new(), viewport, SystemTime::now())
        .expect("bootstrap");
    let (_session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);
    let pane_sizes_before_starving_resize =
        fake_pty_backend.list_pane_sizes(pane_id).expect("resizes");

    let events = runtime.handle_client_resize(client, viewport, Some(PaneArea::Starving));

    assert_eq!(events, Vec::new());
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).expect("resizes"),
        pane_sizes_before_starving_resize
    );
}

/// A re-attach that reports no pane area clears the report the last attach
/// left, and the tab goes back to the viewport minus the two chrome rows.
#[test]
fn a_re_attach_reporting_no_pane_area_replaces_the_earlier_report() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport = Size {
        column_count: 120,
        row_count: 40,
    };
    let client = runtime
        .bootstrap_local(SessionId::new(), viewport, SystemTime::now())
        .expect("bootstrap");
    let (session_id, tab_id, pane_id) = get_only_session_slot(&runtime);

    let reported_pane_size = PaneArea::Reported(Size {
        column_count: 60,
        row_count: 20,
    });
    runtime.handle_client_attach(
        session_id,
        client,
        viewport,
        Some(reported_pane_size),
        tab_id,
        SystemTime::now(),
        false,
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client)
            .expect("client")
            .get_reported_pane_area(),
        Some(reported_pane_size)
    );

    runtime.handle_client_attach(
        session_id,
        client,
        viewport,
        None,
        tab_id,
        SystemTime::now(),
        false,
    );

    let attached_client = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client)
        .expect("client");
    assert_eq!(attached_client.get_reported_pane_area(), None);
    assert_eq!(
        attached_client.get_pane_area(),
        Some(pane_viewport(viewport))
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("resizes")
            .last()
            .unwrap(),
        size_root_pane(pane_id, pane_viewport(viewport), PaneSizing::default())
    );
}

/// A pane command needs the size the tab is drawn at. Its only viewer is
/// starving, so the tab is not viewed at any size and the command rejects.
#[test]
fn a_pane_command_from_a_starving_sole_viewer_is_rejected_as_unviewed() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, left_pane_id);
    register_pane_record(&mut session, right_pane_id);
    register_session_tab(&mut session, tab_id, left_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .unwrap()
        .update_layout(build_horizontal_split(left_pane_id, right_pane_id));
    attach_client_with_reported_pane_area(
        &mut session,
        client_id,
        tab_id,
        Some(left_pane_id),
        Some(PaneArea::Starving),
    );
    runtime.session_by_id.insert(session.session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: Some(left_pane_id),
            direction: Direction::Right,
            resize_amount_cells: 1,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane's tab is not viewed by any client".to_string()),
        }
    );
}

/// Directional focus ranks panes by the rects the tab solves to. Its only
/// viewer is starving, so there is nothing to solve and the move rejects.
#[test]
fn directional_focus_from_a_starving_sole_viewer_is_rejected() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, left_pane_id);
    register_pane_record(&mut session, right_pane_id);
    register_session_tab(&mut session, tab_id, left_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .unwrap()
        .update_layout(build_horizontal_split(left_pane_id, right_pane_id));
    attach_client_with_reported_pane_area(
        &mut session,
        client_id,
        tab_id,
        Some(left_pane_id),
        Some(PaneArea::Starving),
    );
    runtime.session_by_id.insert(session.session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Direction(Direction::Right),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: None,
        }
    );
}
