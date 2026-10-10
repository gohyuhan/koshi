//! Tests for command dispatch: validation rejects ill-formed commands before
//! the match, and every handler the match reaches is
//! exercised — panes, tabs, clients, highlights, fullscreen, detach and the
//! session switch — together with child exits, client attach and detach, the
//! floating pane sizes and cell size those client changes set, and the working
//! directory a new pane opens in.
//!
//! Rejection cases (no context) run against an empty runtime. Cases that need
//! populated state — explicit/default/focused target resolution, in-session-CLI
//! pane defaulting, and `InvalidState` session admission — build sessions with
//! the helpers below and install them into the runtime's `session_by_id` map. Cases
//! that need a live child use [`build_runtime_with_fake`], which hands back the
//! fake backend so spawns, resizes, writes and kills can be read back.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Barrier};
use std::time::{Duration, Instant, SystemTime};

use crate::runtime::pty_inbox::InboxSink;
use koshi_core::command::{
    ClosePaneArgs, CloseTabArgs, CommandSource, CopyArgs, FocusPaneArgs, FocusTabArgs,
    GridPosition, LockModeArgs, MovePaneArgs, MoveTabArgs, NewPaneArgs, NewPanePlacement,
    NewTabArgs, PanePlacementAnchor, PanePlacementTarget, PlacePaneArgs, PlacementRevision,
    ResizePaneArgs, ScrollPaneArgs, Selection, SelectionKind, TabTarget, VisualCommand,
    WriteToPaneArgs,
};
use koshi_core::constant::{GRACEFUL_TIMEOUT_DURATION, MAX_FLOATING_PANES_PER_SESSION};
use koshi_core::geometry::{
    AxisPercent, Direction, FloatingPaneDimension, FloatingPaneSize, PaneArea, PixelCellSize, Size,
    SplitDirection,
};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::process::{ExitStatus, PtySize, ShellKind, SpawnSpec};
use koshi_layout::edit::split_leaf;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::PaneSizing;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use koshi_pane::pane::state::PaneRecord;
use koshi_pty::backend::state::PtyBackend;
use koshi_pty::error::PtyError;
use koshi_session::client::{
    compute_default_pane_area_size, Client, ClientRegistry, FloatingPaneView,
};
use koshi_session::session::pane_ops::NewPaneSpec;
use koshi_session::session::state::{FloatingMember, FloatingPaneSizeSolve, Session, Tab};
use koshi_session::session::tab_ops;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::runtime::event::RuntimeEvent;
use crate::runtime::render_schedule::FRAME_INTERVAL_DURATION;
use koshi_renderer::snapshot::Delivery;

use super::*;
use koshi_core::event::{PaneClosing, PaneCreated, PaneRemoved, QuitCause, TabClosed};

/// A `new-pane` request with nothing chosen: the focused pane of the issuer's
/// tab splits rightward, running the default shell.
fn build_new_pane_args() -> NewPaneArgs {
    NewPaneArgs {
        placement: NewPanePlacement::Split {
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Right,
        },
        working_directory: None,
        spawn_spec: None,
        client_id: None,
    }
}

/// A bare runtime with stub services and no sessions, and the sender
/// that queues events on its inbox.
fn build_runtime() -> (Server, mpsc::Sender<RuntimeEvent>) {
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));
    let runtime = Server::from_runtime_parts(pty_backend, runtime_event_receiver);
    (runtime, runtime_event_sender)
}

/// Like [`build_runtime`], but also hands back the concrete fake backend so a test
/// can drive spawn failures and assert on spawned panes, specs, and resizes.
/// Both the runtime and the returned handle share one backend.
fn build_runtime_with_fake() -> (Server, Arc<FakePtyBackend>, mpsc::Sender<RuntimeEvent>) {
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let runtime = Server::from_runtime_parts(pty_backend, runtime_event_receiver);
    (runtime, fake_pty_backend, runtime_event_sender)
}

/// The id of the single pane of `session_id` that `known_pane_ids` does not
/// list: after one split of a session holding `[root]`, the new pane. Panics
/// unless exactly one such pane exists.
fn find_other_pane_id(server: &Server, session_id: SessionId, known_pane_ids: &[PaneId]) -> PaneId {
    let mut other_pane_ids = server.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .filter(|pane_id| !known_pane_ids.contains(pane_id));
    let other_pane_id = other_pane_ids.next().expect("an unknown pane exists");
    assert_eq!(other_pane_ids.next(), None, "exactly one unknown pane");
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
    CommandEnvelope::from_parts(CommandId::new(), command_source, command)
}

/// Wrap a command in an envelope from an external CLI naming no session and no
/// client, with a fresh id.
fn build_sessionless_cli_command_envelope(command: Command) -> CommandEnvelope {
    build_command_envelope(CommandSource::from_external_cli(None, None), command)
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
    );
    session.request_session_stop();
    session
}

/// Register a fresh `Spawning` pane in the session's registry.
fn register_pane_record(session: &mut Session, pane_id: PaneId) {
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(pane_id))
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

/// Poll the fake backend until `pane_id` records a kill, then return the
/// recorded kill policies. The close handler kills on a detached thread.
/// Panics if no kill arrives within 5 seconds.
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
        let command_envelope = build_sessionless_cli_command_envelope(command);
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
fn clear_selection_cannot_be_issued_from_an_external_cli() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    let command_envelope = build_sessionless_cli_command_envelope(Command::Visual(
        VisualCommand::ClearSelection(ClearSelectionArgs {
            pane_id: PaneId::new(),
        }),
    ));
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
/// client that views the first tab's pane. Every command kind has a structurally
/// complete target here.
struct CommandMatrixFixture {
    runtime: Server,
    session_id: SessionId,
    client_id: ClientId,
    first_tab_id: TabId,
    first_pane_id: PaneId,
    second_tab_id: TabId,
    second_pane_id: PaneId,
}

/// A [`CommandMatrixFixture`] whose client connected from `client_origin`.
fn build_command_matrix_server(client_origin: ClientOrigin) -> CommandMatrixFixture {
    let (mut server, _runtime_event_sender) = build_runtime();
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
    let session_id = session.session_id;
    server.session_by_id.insert(session_id, session);

    CommandMatrixFixture {
        runtime: server,
        session_id,
        client_id,
        first_tab_id,
        first_pane_id,
        second_tab_id,
        second_pane_id,
    }
}

/// How many [`Command`] variants this build has. [`build_every_command`] lists one
/// command of each.
const COMMAND_VARIANT_COUNT: usize = 21;

/// One command of every variant, aimed at `tab_id` and `pane_id` — the tab and pane the
/// acting client of [`build_command_matrix_server`] views. `SwitchSession` names a session
/// id no runtime holds, so it resolves the same way on every runtime.
fn build_every_command(tab_id: TabId, pane_id: PaneId) -> Vec<Command> {
    vec![
        Command::NewPane(build_new_pane_args()),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(pane_id),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
        Command::ResizePane(ResizePaneArgs {
            pane_id: Some(pane_id),
            direction: Direction::Left,
            resize_amount_cells: 5,
        }),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(pane_id),
            client_id: None,
        }),
        Command::NewTab(NewTabArgs::default()),
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(tab_id),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Next,
            client_id: None,
        }),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(pane_id),
            pane_input_bytes: vec![b'x'],
        }),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
        Command::SetLockMode(LockModeArgs {
            is_locked: true,
            client_id: None,
        }),
        Command::ToggleMouseSelect,
        Command::Visual(VisualCommand::ClearSelection(ClearSelectionArgs {
            pane_id,
        })),
        Command::TogglePaneFullscreen,
        Command::MoveTab(MoveTabArgs {
            tab_id: Some(tab_id),
            target_tab_index: 1,
        }),
        Command::MovePane(MovePaneArgs {
            pane_id: Some(pane_id),
            direction: Direction::Right,
        }),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: pane_id,
            placement_target: PanePlacementTarget::Split {
                destination_tab_id: tab_id,
                anchor: PanePlacementAnchor::Tab,
                direction: Direction::Right,
            },
            expected_placement_revision: None,
        }),
        Command::ScrollPane(ScrollPaneArgs {
            pane_id: Some(pane_id),
            scroll_line_count: 3,
        }),
        Command::Quit,
        Command::Detach(DetachArgs { client_id: None }),
        Command::DetachAll,
        Command::SwitchSession(SwitchSessionArgs {
            client_id: None,
            session_id: SessionId::new(),
        }),
    ]
}

/// The variant name of `command`: `Command::Quit` gives `"Quit"`. The match
/// names every variant, so a new variant stops the build here.
fn get_command_name(command: &Command) -> &'static str {
    match command {
        Command::NewPane(_) => "NewPane",
        Command::ClosePane(_) => "ClosePane",
        Command::ResizePane(_) => "ResizePane",
        Command::FocusPane(_) => "FocusPane",
        Command::NewTab(_) => "NewTab",
        Command::CloseTab(_) => "CloseTab",
        Command::FocusTab(_) => "FocusTab",
        Command::WriteToPane(_) => "WriteToPane",
        Command::ToggleLockMode(_) => "ToggleLockMode",
        Command::SetLockMode(_) => "SetLockMode",
        Command::ToggleMouseSelect => "ToggleMouseSelect",
        Command::Visual(_) => "Visual",
        Command::TogglePaneFullscreen => "TogglePaneFullscreen",
        Command::MoveTab(_) => "MoveTab",
        Command::MovePane(_) => "MovePane",
        Command::PlacePane(_) => "PlacePane",
        Command::ScrollPane(_) => "ScrollPane",
        Command::Quit => "Quit",
        Command::Detach(_) => "Detach",
        Command::DetachAll => "DetachAll",
        Command::SwitchSession(_) => "SwitchSession",
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

/// Replace the stored placement revision of `session_id` with
/// `session_revision`, and the one of `client_id` with `client_revision`. The
/// session goes through its serialized form and is inserted back.
fn replace_persisted_placement_revisions(
    runtime: &mut Server,
    session_id: SessionId,
    client_id: ClientId,
    session_revision: u64,
    client_revision: u64,
) {
    let session = runtime
        .session_by_id
        .get(&session_id)
        .expect("session exists");
    let mut serialized_session = serde_json::to_value(session).expect("session serializes");
    serialized_session
        .as_object_mut()
        .expect("session is an object")
        .insert(
            "placement_revision".to_string(),
            serde_json::json!(session_revision),
        );
    let serialized_clients = serialized_session
        .get_mut("clients")
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|clients| clients.get_mut("client_by_id"))
        .and_then(serde_json::Value::as_object_mut)
        .expect("session carries its client map");
    let serialized_client_id = serde_json::to_value(client_id).expect("client id serializes");
    let serialized_client = serialized_clients
        .values_mut()
        .find(|serialized_client| serialized_client.get("client_id") == Some(&serialized_client_id))
        .and_then(serde_json::Value::as_object_mut)
        .expect("client exists");
    serialized_client.insert(
        "placement_revision".to_string(),
        serde_json::json!(client_revision),
    );
    let restored_session = serde_json::from_value(serialized_session).expect("session restores");
    runtime.session_by_id.insert(session_id, restored_session);
}

/// A runtime holding two sessions, each with one tab, one pane and one attached
/// local client. Returns the runtime, the first session's client, tab and pane,
/// and the second session's client. The sender queues events on the inbox.
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
    let CommandMatrixFixture {
        mut runtime,
        first_pane_id: pane_id,
        ..
    } = build_command_matrix_server(ClientOrigin::Local);
    let session_records_before = serialize_session_records(&runtime);

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
        session_records_before,
        "a refused command changed a session"
    );
}

#[test]
fn a_command_from_a_client_id_no_session_holds_is_refused_and_changes_nothing() {
    let CommandMatrixFixture {
        mut runtime,
        first_pane_id: pane_id,
        ..
    } = build_command_matrix_server(ClientOrigin::Local);
    let session_records_before = serialize_session_records(&runtime);

    // The id belongs to no client anywhere, so there is no session to act in.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(ClientId::new()),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(pane_id),
            pane_input_bytes: vec![b'x'],
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
        session_records_before,
        "a refused command changed a session"
    );
}

#[test]
fn a_command_from_a_client_that_has_detached_is_refused_and_changes_nothing() {
    let CommandMatrixFixture {
        mut runtime,
        client_id,
        first_pane_id: pane_id,
        session_id,
        ..
    } = build_command_matrix_server(ClientOrigin::Local);
    let detached_client = runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .detach_client(client_id)
        .expect("the client was attached");
    assert_eq!(detached_client.get_client_id(), client_id);
    let session_records_before = serialize_session_records(&runtime);

    // The client is detached: the session does not hold its id.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(pane_id),
            pane_input_bytes: vec![b'x'],
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
        session_records_before,
        "a refused command changed a session"
    );
}

#[test]
fn a_command_naming_a_client_of_another_session_is_refused_and_changes_nothing() {
    let (mut runtime, _runtime_event_sender, first_client_id, _tab_id, _pane_id, second_client_id) =
        build_two_session_server();
    let session_records_before = serialize_session_records(&runtime);

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
        session_records_before,
        "a refused command changed a session"
    );
}

#[test]
fn a_client_that_connected_from_another_machine_is_admitted_like_a_local_one() {
    let CommandMatrixFixture {
        mut runtime,
        client_id,
        session_id,
        ..
    } = build_command_matrix_server(ClientOrigin::Remote);

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
    let mut listed_command_names = HashSet::new();
    let mut applied_command_names = Vec::new();

    for command_index in 0..COMMAND_VARIANT_COUNT {
        // One fresh runtime per side, so a command that mutates cannot leak
        // into the next command or across the two sides.
        let CommandMatrixFixture {
            runtime: mut local_runtime,
            client_id: local_client_id,
            first_tab_id: local_tab_id,
            first_pane_id: local_pane_id,
            ..
        } = build_command_matrix_server(ClientOrigin::Local);
        let local_command = build_every_command(local_tab_id, local_pane_id)[command_index].clone();
        let command_name = get_command_name(&local_command);
        assert!(
            listed_command_names.insert(command_name),
            "{command_name} is listed twice"
        );
        let local_command_result = local_runtime.dispatch(build_command_envelope(
            CommandSource::from_key_binding(local_client_id),
            local_command,
        ));

        let CommandMatrixFixture {
            runtime: mut remote_runtime,
            client_id: remote_client_id,
            first_tab_id: remote_tab_id,
            first_pane_id: remote_pane_id,
            ..
        } = build_command_matrix_server(ClientOrigin::Remote);
        let remote_command_result = remote_runtime.dispatch(build_command_envelope(
            CommandSource::from_key_binding(remote_client_id),
            build_every_command(remote_tab_id, remote_pane_id)[command_index].clone(),
        ));

        // The whole answer, not only whether it was refused: a refusal matches
        // reason and help text, and a success matches the emitted events one
        // for one, in order.
        let local_outcome = get_command_outcome(&local_command_result);
        assert_eq!(
            get_command_outcome(&remote_command_result),
            local_outcome,
            "{command_name} answered a remote client differently from a local one"
        );

        // The same clients are left attached on both sides, and each remote one
        // still reads back as remote: dispatch never writes the origin.
        assert_eq!(
            list_attached_client_origins(&remote_runtime),
            list_attached_client_origins(&local_runtime)
                .into_iter()
                .map(|_| ClientOrigin::Remote)
                .collect::<Vec<ClientOrigin>>(),
            "{command_name} left a different set of clients attached on the remote side"
        );

        if local_outcome.is_ok() {
            applied_command_names.push(command_name);
        }
    }

    // The commands that reach their handler on this fixture, so the comparison
    // above is not two matching refusals every time. The four missing commands
    // are refused by the fixture or by command admission, identically on both
    // sides: a resize has no border to move in a single-pane tab, a move has
    // no neighbor, a write has no running child, and the switch has no
    // connected viewer to receive the move.
    assert_eq!(
        applied_command_names,
        vec![
            "NewPane",
            "ClosePane",
            "FocusPane",
            "NewTab",
            "CloseTab",
            "FocusTab",
            "ToggleLockMode",
            "SetLockMode",
            "ToggleMouseSelect",
            "Visual",
            "TogglePaneFullscreen",
            "MoveTab",
            "PlacePane",
            "ScrollPane",
            "Quit",
            "Detach",
            "DetachAll",
        ]
    );
}

#[test]
fn explicit_pane_target_absent_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ClosePane(ClosePaneArgs {
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

    // No explicit pane and a CLI source naming no session: nothing to default to.
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ClosePane(ClosePaneArgs::default()));
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

    let command_envelope =
        build_sessionless_cli_command_envelope(Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(PaneId::new()),
            pane_input_bytes: vec![b'x'],
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
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    runtime.show_session_recovery_notice(session_id);

    // An explicit target on a live pane injects the bytes into its child and
    // completes with no events — the write is a side effect, not a state change.
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(split_pane_id),
            pane_input_bytes: vec![b'l', b's', b'\n'],
        }));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert!(emitted_events.is_empty());
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(split_pane_id)
            .unwrap(),
        vec![vec![b'l', b's', b'\n']]
    );
    assert!(!runtime.list_sessions()[&session_id].is_recovery_notice_visible);

    runtime.show_session_recovery_notice(session_id);
    let empty_write =
        build_sessionless_cli_command_envelope(Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(split_pane_id),
            pane_input_bytes: Vec::new(),
        }));
    let empty_write_command_id = empty_write.command_id;
    assert_eq!(
        runtime.dispatch(empty_write),
        CommandResult::Ok {
            command_id: empty_write_command_id,
            emitted_events: Vec::new(),
        }
    );
    assert!(runtime.list_sessions()[&session_id].is_recovery_notice_visible);

    fake_pty_backend.fail_writes_on(
        split_pane_id,
        PtyError::UnknownPane {
            pane_id: split_pane_id,
        },
    );
    let failed_write =
        build_sessionless_cli_command_envelope(Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(split_pane_id),
            pane_input_bytes: vec![b'x'],
        }));
    let failed_write_command_id = failed_write.command_id;
    assert_eq!(
        runtime.dispatch(failed_write),
        CommandResult::Rejected {
            command_id: failed_write_command_id,
            reason: RejectReason::InvalidState,
            help: Some("pane is not accepting input".to_string()),
        }
    );
    assert!(runtime.list_sessions()[&session_id].is_recovery_notice_visible);
}

/// A commanded write has no visibility guard: the bytes reach the pane's child
/// even when the layout has no room to draw the pane. `Server::find_typed_pane`
/// refuses an undrawn pane; this path does not.
#[test]
fn write_to_a_suppressed_pane_still_reaches_its_shell() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();

    // Shrink the client's terminal until the tab has no room to draw its panes.
    runtime.handle_client_resize(
        client_id,
        Size {
            column_count: 3,
            row_count: 3,
        },
        None,
        None,
    );
    assert!(
        runtime
            .build_snapshot(client_id)
            .expect("snapshot")
            .session_snapshot
            .active_tab_snapshot
            .is_every_pane_suppressed,
        "test setup: the panes must be suppressed at this size"
    );

    let command_envelope =
        build_sessionless_cli_command_envelope(Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(split_pane_id),
            pane_input_bytes: vec![b'l', b's'],
        }));
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![])
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(split_pane_id)
            .unwrap(),
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
/// output, the same as typing the bytes into the pane. A write from a CLI
/// source naming no client moves no view.
#[test]
fn a_client_sourced_write_to_pane_snaps_that_client_view_to_live_output() {
    let ResizeFixture {
        mut runtime,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    runtime.handle_pty_output(split_pane_id, &b"\n".repeat(200)); // push lines into history
    runtime.scroll_up(client_id, split_pane_id, 3);
    assert_eq!(
        get_client_scroll_offset(&runtime, client_id, split_pane_id),
        3
    );

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(split_pane_id),
            pane_input_bytes: vec![b'l', b's', b'\n'],
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![])
    );
    assert_eq!(
        get_client_scroll_offset(&runtime, client_id, split_pane_id),
        0
    );
}

/// A client-sourced write drops that client's highlight in the target pane,
/// the same as typing over a selection, and leaves the view at live output.
#[test]
fn a_client_sourced_write_clears_the_clients_highlight_in_the_pane() {
    let ResizeFixture {
        mut runtime,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    runtime.handle_pty_output(split_pane_id, &b"\n".repeat(200));
    runtime.scroll_up(client_id, split_pane_id, 3);
    runtime
        .get_client_mut(client_id)
        .unwrap()
        .set_selection(split_pane_id, build_test_selection());

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(split_pane_id),
            pane_input_bytes: vec![b'l', b's', b'\n'],
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![])
    );

    assert_eq!(
        get_client_scroll_offset(&runtime, client_id, split_pane_id),
        0
    );
    let client = runtime
        .list_sessions()
        .values()
        .next()
        .unwrap()
        .clients
        .get_client_by_id(client_id)
        .unwrap();
    assert_eq!(client.get_selection(split_pane_id), None);
}

/// An empty payload sends no bytes to the child, so it is not input: it leaves a
/// parked scrollback view exactly where it was.
#[test]
fn an_empty_client_sourced_write_leaves_a_parked_view_alone() {
    let ResizeFixture {
        mut runtime,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    runtime.handle_pty_output(split_pane_id, &b"\n".repeat(200));
    runtime.scroll_up(client_id, split_pane_id, 3);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(split_pane_id),
            pane_input_bytes: Vec::new(),
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![])
    );
    assert_eq!(
        get_client_scroll_offset(&runtime, client_id, split_pane_id),
        3
    );
}

#[test]
fn write_to_pane_defaults_to_the_clients_focused_pane() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();

    // No explicit target: a keybinding source writes to the client's focused
    // pane, which the split left on `split_pane_id`.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: None,
            pane_input_bytes: vec![b'a'],
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![])
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(split_pane_id)
            .unwrap(),
        vec![vec![b'a']]
    );
}

#[test]
fn write_to_pane_via_in_session_cli_defaults_to_the_issuing_pane() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();

    // Issued from inside `split_pane_id` with no explicit target: the captured
    // issuing pane is the target.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(client_id),
        split_pane_id,
        PathBuf::from("/sock"),
    );
    let command_envelope = build_command_envelope(
        command_source,
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: None,
            pane_input_bytes: vec![b'b'],
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![])
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(split_pane_id)
            .unwrap(),
        vec![vec![b'b']]
    );
}

#[test]
fn write_to_an_exited_pane_is_rejected() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        split_pane_id,
        ..
    } = build_resize_fixture();

    // Drive the live split to `Exited`; a dead pane takes no input.
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .panes
        .get_pane_record_mut_by_id(split_pane_id)
        .unwrap()
        .update_lifecycle(PaneLifecycleEvent::ProcessExited {
            exit_code: Some(0),
            exited_at: SystemTime::now(),
        })
        .unwrap();

    let command_envelope =
        build_sessionless_cli_command_envelope(Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(split_pane_id),
            pane_input_bytes: vec![b'x'],
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
        .list_pane_write_bytes(split_pane_id)
        .unwrap()
        .is_empty());
}

#[test]
fn write_to_a_closing_pane_is_rejected() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        split_pane_id,
        ..
    } = build_resize_fixture();

    // A pane mid-teardown (`Closing`) takes no input.
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .panes
        .get_pane_record_mut_by_id(split_pane_id)
        .unwrap()
        .update_lifecycle(PaneLifecycleEvent::CloseRequested {
            close_requested_at: SystemTime::now(),
        })
        .unwrap();

    let command_envelope =
        build_sessionless_cli_command_envelope(Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(split_pane_id),
            pane_input_bytes: vec![b'x'],
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
        .list_pane_write_bytes(split_pane_id)
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

    let command_envelope =
        build_sessionless_cli_command_envelope(Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(pane_id),
            pane_input_bytes: vec![b'x'],
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
fn write_with_empty_pane_input_is_a_noop_ok() {
    let ResizeFixture {
        mut runtime,
        split_pane_id,
        ..
    } = build_resize_fixture();

    // An empty payload is a legal no-op write: it applies with no events.
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(split_pane_id),
            pane_input_bytes: Vec::new(),
        }));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert!(emitted_events.is_empty());
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
}

#[test]
fn resize_pane_default_target_without_context_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ResizePane(ResizePaneArgs {
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

    // A CLI source naming no session has no session context to resolve a tab within.
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
        let command_envelope = build_sessionless_cli_command_envelope(command);
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

    // These create within a session; a CLI source naming no session resolves to
    // no session, so there is nothing to act on.
    let command_records = vec![
        Command::NewTab(NewTabArgs::default()),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Right,
            },
            working_directory: None,
            spawn_spec: Some(build_spawn_spec()),
            client_id: None,
        }),
    ];

    for command in command_records {
        let command_envelope = build_sessionless_cli_command_envelope(command);
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

    let command_envelope = build_sessionless_cli_command_envelope(Command::NewPane(NewPaneArgs {
        placement: NewPanePlacement::Split {
            source_pane_id: Some(PaneId::new()),
            tab_id: None,
            direction: Direction::Right,
        },
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
    // a CLI source naming no session has no focused pane to anchor on. The stacked shape
    // resolves its anchor the same way, so it rejects identically.
    let command_cases = vec![
        build_new_pane_args(),
        NewPaneArgs {
            placement: NewPanePlacement::Stacked {
                source_pane_id: None,
                tab_id: None,
            },
            ..build_new_pane_args()
        },
    ];

    for command_args in command_cases {
        let command_envelope =
            build_sessionless_cli_command_envelope(Command::NewPane(command_args));
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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneCreated", "LayoutChanged", "PaneFocused", "PtyResized"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    // The split registered a second pane in the tab.
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .count_pane_records(),
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
            placement: NewPanePlacement::Split {
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Down,
            },
            ..build_new_pane_args()
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );

    let new_pane_id = find_other_pane_id(&runtime, session_id, &[pane_id]);
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
            placement: NewPanePlacement::Stacked {
                source_pane_id: None,
                tab_id: None,
            },
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneCreated", "LayoutChanged", "PaneFocused", "PtyResized"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let new_pane_id = find_other_pane_id(&runtime, session_id, &[pane_id]);
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
            .get_focused_pane_id(tab_id),
        Some(new_pane_id)
    );
    assert!(runtime.live_pane_ids.contains(&new_pane_id));
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
            placement: NewPanePlacement::Stacked {
                source_pane_id: None,
                tab_id: None,
            },
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneCreated", "LayoutChanged", "PaneFocused", "PtyResized"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let new_pane_id = find_other_pane_id(
        &runtime,
        session_id,
        &[first_stack_pane_id, second_stack_pane_id],
    );
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
            .get_focused_pane_id(tab_id),
        Some(new_pane_id)
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
            placement: NewPanePlacement::Stacked {
                source_pane_id: Some(PaneId::new()),
                tab_id: None,
            },
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
            placement: NewPanePlacement::Stacked {
                source_pane_id: None,
                tab_id: None,
            },
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinimumSize,
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
    let layout_before_spawn = runtime.session_by_id[&session_id].tabs[&tab_id]
        .get_layout_tree()
        .clone();

    // Launch-then-commit holds for the stacked shape too: the child cannot
    // launch, so no stack is created and nothing is committed.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Stacked {
                source_pane_id: None,
                tab_id: None,
            },
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
        runtime.session_by_id[&session_id]
            .panes
            .count_pane_records(),
        1
    );
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &layout_before_spawn
    );
    assert!(runtime.live_pane_ids.is_empty());
    assert!(fake_pty_backend.list_spawned_pane_ids().is_empty());
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(tab_id),
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
            reason: RejectReason::MinimumSize,
            help: Some("not enough space for a new pane".to_string()),
        }
    );
}

#[test]
fn new_pane_explicit_pane_in_session_without_clients_is_rejected() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // The first session holds the acting client.
    let client_id = ClientId::new();
    let first_session_id = SessionId::new();
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut first_session = build_bare_session(first_session_id);
    register_pane_record(&mut first_session, first_pane_id);
    register_session_tab(&mut first_session, first_tab_id, first_pane_id);
    attach_client(
        &mut first_session,
        client_id,
        first_tab_id,
        Some(first_pane_id),
    );
    runtime
        .session_by_id
        .insert(first_session_id, first_session);

    // The second session owns the explicit `--pane` target and has no client.
    let second_session_id = SessionId::new();
    let second_tab_id = TabId::new();
    let second_pane_id = PaneId::new();
    let mut second_session = build_bare_session(second_session_id);
    register_pane_record(&mut second_session, second_pane_id);
    register_session_tab(&mut second_session, second_tab_id, second_pane_id);
    runtime
        .session_by_id
        .insert(second_session_id, second_session);

    // A global `--pane` targets the second session, which has no client to view
    // the tab: the command is rejected.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(second_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
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
        runtime.session_by_id[&second_session_id]
            .panes
            .count_pane_records(),
        1
    );
    assert_eq!(
        runtime.session_by_id[&first_session_id]
            .panes
            .count_pane_records(),
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
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ClosePane(ClosePaneArgs {
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
        runtime.session_by_id[&session_id]
            .panes
            .count_pane_records(),
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

    // A CLI source naming no session has no acting session, so admission is reached only
    // via the pane's owning session — which is stopping.
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ClosePane(ClosePaneArgs {
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);

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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
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
            .get_focused_pane_id(tab_id),
        Some(root_pane_id)
    );
    assert!(!runtime.live_pane_ids.contains(&new_pane_id));
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
fn build_lock_fixture() -> (Server, mpsc::Sender<RuntimeEvent>, ClientId, SessionId) {
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
fn get_client_lock_mode(runtime: &Server, session_id: SessionId, client_id: ClientId) -> LockMode {
    runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_lock_mode()
}

#[test]
fn toggle_lock_mode_locks_an_unlocked_client() {
    let (mut runtime, _runtime_event_sender, client_id, session_id) = build_lock_fixture();

    // A default-Normal client toggles into Locked: exactly one event.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(list_event_names(&emitted_events), ["InputModeChanged"]);
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, client_id),
        LockMode::Locked
    );
}

#[test]
fn toggle_mouse_select_flips_the_client_flag() {
    let (mut runtime, _runtime_event_sender, client_id, session_id) = build_lock_fixture();
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
    let (mut runtime, _runtime_event_sender, client_id, session_id) = build_lock_fixture();
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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(list_event_names(&emitted_events), ["InputModeChanged"]);
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, client_id),
        LockMode::Normal
    );
}

#[test]
fn set_lock_mode_locks_then_unlocks() {
    let (mut runtime, _runtime_event_sender, client_id, session_id) = build_lock_fixture();

    let lock_command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::SetLockMode(LockModeArgs {
            is_locked: true,
            client_id: None,
        }),
    );
    let lock_command_id = lock_command_envelope.command_id;
    match runtime.dispatch(lock_command_envelope) {
        CommandResult::Ok {
            command_id,
            emitted_events,
        } => {
            assert_eq!(command_id, lock_command_id);
            assert_eq!(list_event_names(&emitted_events), ["InputModeChanged"]);
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, client_id),
        LockMode::Locked
    );

    let unlock_command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::SetLockMode(LockModeArgs {
            is_locked: false,
            client_id: None,
        }),
    );
    let unlock_command_id = unlock_command_envelope.command_id;
    match runtime.dispatch(unlock_command_envelope) {
        CommandResult::Ok {
            command_id,
            emitted_events,
        } => {
            assert_eq!(command_id, unlock_command_id);
            assert_eq!(list_event_names(&emitted_events), ["InputModeChanged"]);
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, client_id),
        LockMode::Normal
    );
}

#[test]
fn setting_the_current_lock_mode_emits_nothing() {
    let (mut runtime, _runtime_event_sender, client_id, session_id) = build_lock_fixture();

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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(emitted_events, Vec::new());
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, client_id),
        LockMode::Normal
    );
}

#[test]
fn lock_mode_is_isolated_between_clients() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let (alice_client_id, bob_client_id) = (ClientId::new(), ClientId::new());
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    // Both clients view the same tab and pane, proving lock is per-client, not
    // per-pane.
    attach_client(&mut session, alice_client_id, tab_id, Some(pane_id));
    attach_client(&mut session, bob_client_id, tab_id, Some(pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(alice_client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["InputModeChanged"])
    );

    assert_eq!(
        get_client_lock_mode(&runtime, session_id, alice_client_id),
        LockMode::Locked
    );
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, bob_client_id),
        LockMode::Normal
    );
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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(list_event_names(&emitted_events), ["InputModeChanged"]);
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, client_id),
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["SelectionChanged"])
    );

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

    // The drag names a pane that the session does not hold.
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["SelectionChanged"])
    );
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
    // block and is written whole. A pasted Tab reaches the shell and fires no
    // tab-switch binding.
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();

    runtime.handle_host_paste(client_id, "ls\ttmp\ncat");
    let input_write_batches = fake_pty_backend
        .list_pane_write_bytes(split_pane_id)
        .expect("pane writes");
    assert_eq!(
        input_write_batches.last().expect("one write"),
        b"ls\ttmp\rcat",
        "raw bytes, line break as the Enter byte"
    );
}

#[test]
fn a_host_paste_wraps_in_bracketed_markers_when_the_pane_turned_them_on() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    runtime.handle_pty_output(split_pane_id, b"\x1b[?2004h");

    runtime.handle_host_paste(client_id, "ok");
    let input_write_batches = fake_pty_backend
        .list_pane_write_bytes(split_pane_id)
        .expect("pane writes");
    assert_eq!(
        input_write_batches.last().expect("one write"),
        b"\x1b[200~ok\x1b[201~"
    );
}

#[test]
fn a_host_paste_clears_the_highlight_in_the_pasted_pane() {
    let ResizeFixture {
        mut runtime,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    let client = runtime.get_client_mut(client_id).expect("client");
    client.set_selection(split_pane_id, build_test_selection());

    runtime.handle_host_paste(client_id, "x");
    assert_eq!(
        runtime
            .get_client_mut(client_id)
            .expect("client")
            .get_selection(split_pane_id),
        None,
        "pasted text reached the child, so the highlight is gone"
    );
}

#[test]
fn copying_a_pane_with_no_highlight_writes_nothing_and_is_not_an_error() {
    // A plain click ends a gesture that highlighted nothing, so the copy it
    // dispatches finds nothing to copy. That is a no-op, not a rejection.
    let ResizeFixture {
        mut runtime,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();

    let command_envelope = build_command_envelope(
        CommandSource::from_mouse(client_id),
        Command::Visual(VisualCommand::Copy(CopyArgs {
            pane_id: split_pane_id,
            should_trim_trailing_whitespace: true,
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

/// A character highlight from row 0, column 0 to row 0, column 1.
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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(emitted_events, Vec::new());
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
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

    // A CLI source naming no session and no client: quit ends the process and
    // detaches no client.
    let command_envelope = build_sessionless_cli_command_envelope(Command::Quit);
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

    // A CLI source naming no session and no client: the resolver has no
    // session to find a target client in.
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::FocusPane(FocusPaneArgs {
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
fn directional_focus_follows_the_screen_up_then_left_across_the_four_pane_fixture() {
    // `L | U U` above `L | M R` at 80 by 24 cells: `left_pane_id` (L) spans
    // both rows, `upper_right_pane_id` (U) spans the upper right, and
    // `lower_middle_pane_id` (M) and `lower_right_pane_id` (R) share the lower
    // right. Up from R lands on U; Left from U lands on L.
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let (left_pane_id, upper_right_pane_id, lower_middle_pane_id, lower_right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    for pane_id in [
        left_pane_id,
        upper_right_pane_id,
        lower_middle_pane_id,
        lower_right_pane_id,
    ] {
        register_pane_record(&mut session, pane_id);
    }
    register_session_tab(&mut session, tab_id, left_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode {
            direction: SplitDirection::Horizontal,
            children: vec![
                LayoutNode::Pane(left_pane_id),
                LayoutNode::Split(SplitNode::with_equal_weights(
                    SplitDirection::Vertical,
                    vec![
                        LayoutNode::Pane(upper_right_pane_id),
                        LayoutNode::Split(SplitNode::with_equal_weights(
                            SplitDirection::Horizontal,
                            vec![
                                LayoutNode::Pane(lower_middle_pane_id),
                                LayoutNode::Pane(lower_right_pane_id),
                            ],
                        )),
                    ],
                )),
            ],
            weights: vec![
                koshi_layout::size::SizeWeight::from_primary_constraint(
                    koshi_layout::size::SizeConstraint::Flex(1),
                ),
                koshi_layout::size::SizeWeight::from_primary_constraint(
                    koshi_layout::size::SizeConstraint::Flex(2),
                ),
            ],
            active_child_index: 0,
        }));
    attach_client(&mut session, client_id, tab_id, Some(lower_right_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    for (direction, expected_focused_pane_id) in [
        (Direction::Up, upper_right_pane_id),
        (Direction::Left, left_pane_id),
    ] {
        let command_envelope = build_command_envelope(
            CommandSource::from_key_binding(client_id),
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Direction(direction),
                client_id: None,
            }),
        );
        let command_id = command_envelope.command_id;
        match runtime.dispatch(command_envelope) {
            CommandResult::Ok {
                command_id: ok_command_id,
                emitted_events,
            } => {
                assert_eq!(ok_command_id, command_id);
                assert_eq!(list_event_names(&emitted_events), ["PaneFocused"]);
            }
            unexpected_result => panic!("expected Ok for {direction:?}, got {unexpected_result:?}"),
        }
        assert_eq!(
            runtime.session_by_id[&session_id]
                .clients
                .get_client_by_id(client_id)
                .expect("client")
                .get_focused_pane_id(tab_id),
            Some(expected_focused_pane_id),
            "{direction:?}"
        );
    }
}

#[test]
fn focus_pane_moves_focus_records_mru_and_emits_one_event() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let observer_client_id = ClientId::new();
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
    attach_client(
        &mut session,
        observer_client_id,
        tab_id,
        Some(target_pane_id),
    );
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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            // Exactly the focus fact: a plain move changes no layout and no PTY.
            assert_eq!(list_event_names(&emitted_events), ["PaneFocused"]);
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane_id(tab_id),
        Some(target_pane_id)
    );
    assert_eq!(
        session.tabs[&tab_id].list_focus_mru().first(),
        Some(&target_pane_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("focused client")
            .get_placement_revision(),
        1
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(observer_client_id)
            .expect("observer client")
            .get_placement_revision(),
        0
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
            .get_focused_pane_id(tab_id),
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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            // LayoutChanged (the stack swapped members) + PaneFocused. No
            // PtyResized: neither pane has a live PTY here.
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PaneFocused"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
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
            .get_focused_pane_id(tab_id),
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs[&tab_id].get_layout_tree(), &layout);
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane_id(tab_id),
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
            // LayoutChanged only: the stack expands `collapsed_pane_id`, and
            // the focus does not move, so no PaneFocused is emitted.
            assert_eq!(list_event_names(&emitted_events), ["LayoutChanged"]);
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
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
            .get_focused_pane_id(tab_id),
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(target_client_id)
            .expect("target client")
            .get_focused_pane_id(target_tab_id),
        Some(target_pane_id)
    );
    // The issuer's own focus is untouched.
    assert_eq!(
        session
            .clients
            .get_client_by_id(issuer_client_id)
            .expect("issuer client")
            .get_focused_pane_id(issuer_tab_id),
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
            .get_focused_pane_id(tab_id),
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane_id(tab_id),
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane_id(tab_id),
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let stacked_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Stacked {
                source_pane_id: None,
                tab_id: None,
            },
            ..build_new_pane_args()
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let third_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id, stacked_pane_id]);
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
            // LayoutChanged + PtyResized(stacked_pane_id) + PaneFocused. The third pane collapses
            // to a header and keeps its last PTY size, so it is not resized.
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PtyResized", "PaneFocused"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
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
            .get_focused_pane_id(tab_id),
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
    let claimed_session_id = SessionId::new();
    let other_session_pane_id = PaneId::new();

    let mut claimed_session = build_bare_session(claimed_session_id);
    attach_client(&mut claimed_session, client_id, TabId::new(), None);
    runtime
        .session_by_id
        .insert(claimed_session.session_id, claimed_session);

    // The pane lives in the other session. The claimed session holds no such
    // source pane: the command is refused before any target resolution.
    let mut other_session = build_bare_session(SessionId::new());
    register_pane_record(&mut other_session, other_session_pane_id);
    runtime
        .session_by_id
        .insert(other_session.session_id, other_session);

    let command_source = CommandSource::from_in_session_cli(
        claimed_session_id,
        Some(client_id),
        other_session_pane_id,
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);

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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);

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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "LayoutChanged",
            "PaneFocused"
        ])
    );
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
    // The pane has a close request: a command issued from it is refused.
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
    // A control connection that presented `CommandSource::KeyBinding` reaches
    // dispatch as `ExternalCli { session_id: None, target_client_id: None }`, so
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
    // The source names a session that this runtime does not run.
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
    let focused_elsewhere_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, active_tab_pane_id);
    // `focused_elsewhere_pane_id` is in the registry but not in the active tab's layout.
    register_pane_record(&mut session, focused_elsewhere_pane_id);
    register_session_tab(&mut session, tab_id, active_tab_pane_id);
    attach_client(
        &mut session,
        client_id,
        tab_id,
        Some(focused_elsewhere_pane_id),
    );
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
fn new_pane_with_command_carries_the_working_directory_into_it() {
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

    // The command carries `working_directory` `/work`.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Right,
            },
            working_directory: Some(PathBuf::from("/work")),
            spawn_spec: Some(build_spawn_spec()),
            client_id: None,
        }),
    ));

    // `--cwd` reaches the spawned child and is recorded as the pane's directory.
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
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
fn in_session_cli_session_id_is_authoritative_over_a_mismatched_client() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let claimed_session_id = SessionId::new();
    let source_pane_id = PaneId::new();
    let tab_id = TabId::new();

    // The source claims one session, and the client is attached to another
    // session. The claimed session is looked up by its id, and the
    // client-scoped command checks the client there before it acts.
    let mut claimed_session = build_bare_session(claimed_session_id);
    register_pane_record(&mut claimed_session, source_pane_id);
    register_session_tab(&mut claimed_session, tab_id, source_pane_id);
    runtime
        .session_by_id
        .insert(claimed_session_id, claimed_session);
    let mut attached_session = build_bare_session(SessionId::new());
    attach_client(&mut attached_session, client_id, TabId::new(), None);
    runtime
        .session_by_id
        .insert(attached_session.session_id, attached_session);

    let command_source = CommandSource::from_in_session_cli(
        claimed_session_id,
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
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
            focus_target: TabTarget::Previous,
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
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(session_id);
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    // The client's active tab is `second_tab_id`. The CLI command comes from
    // `first_pane_id`, in `first_tab_id`.
    attach_client(&mut session, client_id, second_tab_id, None);
    runtime.session_by_id.insert(session.session_id, session);

    // CloseTab with no explicit tab: an in-session CLI source resolves to the
    // tab that holds `first_pane_id`, not to the client's active tab.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(client_id),
        first_pane_id,
        PathBuf::from("/sock"),
    );
    let command_result = runtime.dispatch(build_command_envelope(
        command_source,
        Command::CloseTab(CloseTabArgs::default()),
    ));
    assert_eq!(
        get_command_outcome(&command_result),
        Ok(vec!["PaneClosing", "PaneRemoved", "TabClosed"])
    );

    // The source pane's tab is closed. The client's active tab stays
    // `second_tab_id`.
    let session = &runtime.session_by_id[&session_id];
    assert!(!session.tabs.contains_key(&first_tab_id));
    assert!(session.tabs.contains_key(&second_tab_id));
    assert!(session.panes.get_pane_record_by_id(first_pane_id).is_none());
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        second_tab_id
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

    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
    match command_result {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                vec!["PaneCreated", "LayoutChanged", "PaneFocused", "PtyResized"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
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
    assert!(runtime.live_pane_ids.contains(&new_pane_id));
    // The root has no PTY yet, so it is neither spawned nor parked.
    assert!(!runtime.live_pane_ids.contains(&root_pane_id));
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
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
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
    // identity and the in-session identity vars added to its environment, and
    // recorded on the pane without the in-session identity vars.
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
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
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(new_pane_id)
            .unwrap()
            .spawn_spec,
        Some(recorded_spawn_spec)
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(tab_id),
        Some(new_pane_id)
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
    let layout_before_spawn = runtime.session_by_id[&session_id].tabs[&tab_id]
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
        runtime.session_by_id[&session_id]
            .panes
            .count_pane_records(),
        1
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(root_pane_id)
            .map(PaneRecord::get_lifecycle),
        Some(&PaneLifecycle::Spawning)
    );
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &layout_before_spawn
    );
    assert!(runtime.live_pane_ids.is_empty());
    assert!(fake_pty_backend.list_spawned_pane_ids().is_empty());
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(tab_id),
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
    let layout_before_spawn = runtime.session_by_id[&session_id].tabs[&back_tab_id]
        .get_layout_tree()
        .clone();

    // The spawn runs before the client moves onto the background tab, and the
    // spawn fails: the client stays on the front tab, and no pane appears.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(back_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
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
            .get_active_tab_id(),
        front_tab_id
    );
    assert_eq!(session.panes.count_pane_records(), 2);
    assert_eq!(
        session.tabs[&back_tab_id].get_layout_tree(),
        &layout_before_spawn
    );
    assert!(runtime.live_pane_ids.is_empty());
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
            placement: NewPanePlacement::Split {
                source_pane_id: Some(back_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            ..build_new_pane_args()
        }),
    ));

    // No client was viewing the background tab, so the sole client is adopted
    // onto it: it switches to view that tab, the split spawns like any in-view
    // one, and the adopted client focuses the new pane. Events: TabFocused,
    // PaneCreated, LayoutChanged, PaneFocused, PtyResized (the PTY-less
    // `pane_back` sibling is skipped by the reflow).
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[front_pane_id, back_pane_id]);
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        back_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(back_tab_id),
        Some(new_pane_id)
    );
    assert!(runtime.live_pane_ids.contains(&new_pane_id));
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

    // Two clients in one session, both on the front tab. The issuer is the
    // client with the higher id: the adoption picks the issuer, not the client
    // with the lowest id.
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
            placement: NewPanePlacement::Split {
                source_pane_id: Some(back_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let new_pane_id = find_other_pane_id(&runtime, session_id, &[front_pane_id, back_pane_id]);
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(issuer_client_id)
            .unwrap()
            .get_active_tab_id(),
        back_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(issuer_client_id)
            .unwrap()
            .get_focused_pane_id(back_tab_id),
        Some(new_pane_id)
    );
    // The bystander was left exactly where it was.
    assert_eq!(
        session
            .clients
            .get_client_by_id(bystander_client_id)
            .unwrap()
            .get_active_tab_id(),
        front_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(bystander_client_id)
            .unwrap()
            .get_focused_pane_id(back_tab_id),
        None
    );
    assert!(runtime.live_pane_ids.contains(&new_pane_id));
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

    // An external source with no issuing client, a background tab with no
    // viewer, and two attached clients: the command is rejected with a request
    // for a named target, and nothing changes.
    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(back_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
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
    assert_eq!(session.panes.count_pane_records(), 2);
    assert_eq!(
        session
            .clients
            .get_client_by_id(first_client_id)
            .unwrap()
            .get_active_tab_id(),
        front_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(second_client_id)
            .unwrap()
            .get_active_tab_id(),
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
            placement: NewPanePlacement::Split {
                source_pane_id: Some(back_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            client_id: Some(target_client_id),
            ..build_new_pane_args()
        }),
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[front_pane_id, back_pane_id]);
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(target_client_id)
            .unwrap()
            .get_active_tab_id(),
        back_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(target_client_id)
            .unwrap()
            .get_focused_pane_id(back_tab_id),
        Some(new_pane_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(bystander_client_id)
            .unwrap()
            .get_active_tab_id(),
        front_tab_id
    );
    assert!(runtime.live_pane_ids.contains(&new_pane_id));
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
            placement: NewPanePlacement::Split {
                source_pane_id: Some(back_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
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
        runtime.session_by_id[&session_id]
            .panes
            .count_pane_records(),
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

    // The issuer runs the command and names `other_client_id` as the target
    // client: the explicit `--client` wins even in-session, so `other_client_id`
    // focuses the new pane and the issuer's focus stays where it was.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(issuer_client_id),
        Command::NewPane(NewPaneArgs {
            client_id: Some(other_client_id),
            ..build_new_pane_args()
        }),
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(other_client_id)
            .unwrap()
            .get_focused_pane_id(tab_id),
        Some(new_pane_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(issuer_client_id)
            .unwrap()
            .get_focused_pane_id(tab_id),
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
        runtime.session_by_id[&session_id]
            .panes
            .count_pane_records(),
        1
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(issuer_client_id)
            .unwrap()
            .get_focused_pane_id(tab_id),
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

    // The split sizes against the 2x1 viewport of the client to adopt. The fit
    // check runs before any change: the pane does not fit, the command is
    // rejected, and the client stays on its tab.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(back_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinimumSize,
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
            .get_active_tab_id(),
        front_tab_id
    );
    assert_eq!(session.panes.count_pane_records(), 2);
    assert!(fake_pty_backend.list_spawned_pane_ids().is_empty());
    assert!(runtime.live_pane_ids.is_empty());
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
    let mut narrow_viewer_client = Client::from_attachment(
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
    narrow_viewer_client.update_focused_pane(front_tab_id, front_pane_id);
    session.attach_client(narrow_viewer_client);
    let wide_viewer_client_id = ClientId::new();
    attach_client(
        &mut session,
        wide_viewer_client_id,
        front_tab_id,
        Some(front_pane_id),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The wide viewer splits the front tab. The split pane's PTY is sized to
    // the narrow viewer's 40x8 pane region: its right half, 20x8, holds 18x6.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(wide_viewer_client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let split_pane_id = find_other_pane_id(&runtime, session_id, &[front_pane_id, back_pane_id]);
    let pane_size_history_before = fake_pty_backend.list_pane_sizes(split_pane_id).unwrap();

    // The narrow viewer splits the background tab: it moves onto that tab and
    // leaves the front tab, whose pane region grows to the wide viewer's 80x22.
    // The front tab's live PTY is resized once, to 38x20.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(narrow_viewer_client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(back_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            ..build_new_pane_args()
        }),
    ));
    assert_eq!(
        pane_size_history_before,
        vec![PtySize {
            column_count: 18,
            row_count: 6
        }]
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(split_pane_id).unwrap(),
        vec![
            PtySize {
                column_count: 18,
                row_count: 6
            },
            PtySize {
                column_count: 38,
                row_count: 20
            }
        ]
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
    let split_pane_id = find_other_pane_id(&runtime, session_id, &[front_pane_id, back_pane_id]);
    let resize_count_before = fake_pty_backend
        .list_pane_sizes(split_pane_id)
        .unwrap()
        .len();

    // The sole viewer is adopted onto the background tab, leaving the front tab
    // with no viewer: its live PTY keeps its size — not resized at all.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(back_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            ..build_new_pane_args()
        }),
    ));
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
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
    let mut narrow_viewer_client = Client::from_attachment(
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
    narrow_viewer_client.update_focused_pane(back_tab_id, back_pane_id);
    session.attach_client(narrow_viewer_client);
    let wide_viewer_client_id = ClientId::new();
    let mut wide_viewer_client = Client::from_attachment(
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
    wide_viewer_client.update_focused_pane(front_tab_id, front_pane_id);
    session.attach_client(wide_viewer_client);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The narrow viewer splits the back tab while it is the only 40-wide viewer: the sibling's
    // PTY is sized to 40 wide.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(narrow_viewer_client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let split_pane_id = find_other_pane_id(&runtime, session_id, &[back_pane_id, front_pane_id]);
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
        .update_active_tab_id(front_tab_id);

    // The wide viewer splits `pane_back` on the now-background tab and is adopted at 100 wide. The
    // untouched sibling `split_pane_id` must be reflowed to the larger geometry, not left at its
    // stale 40-wide size.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(wide_viewer_client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(back_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
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
    // so it rejects MinimumSize — before any mutation.
    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(back_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            ..build_new_pane_args()
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinimumSize,
            help: Some("not enough space for a new pane".to_string()),
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .count_pane_records(),
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
    let first_split_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let second_split_pane_id =
        find_other_pane_id(&runtime, session_id, &[root_pane_id, first_split_pane_id]);
    let first_split_pane_resize_count_before = fake_pty_backend
        .list_pane_sizes(first_split_pane_id)
        .unwrap()
        .len();

    // Split 3 nests another pane under the second split pane: its 20-column
    // quarter halves, and its PTY goes from 18x20 to 8x20. The first split pane
    // keeps its rectangle and PTY size.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(first_split_pane_id)
            .unwrap()
            .len(),
        first_split_pane_resize_count_before
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(second_split_pane_id)
            .unwrap(),
        vec![
            PtySize {
                column_count: 18,
                row_count: 20
            },
            PtySize {
                column_count: 8,
                row_count: 20
            }
        ]
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
    let first_split_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
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
    match command_result {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                vec!["PaneCreated", "LayoutChanged", "PaneFocused", "PtyResized"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    let second_split_pane_id =
        find_other_pane_id(&runtime, session_id, &[root_pane_id, first_split_pane_id]);
    assert!(runtime.live_pane_ids.contains(&second_split_pane_id));
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
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
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
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
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

    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
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
            placement: NewPanePlacement::Split {
                source_pane_id: Some(root_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            ..build_new_pane_args()
        }),
    ));
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
    let session = &runtime.session_by_id[&session_id];
    assert!(runtime.live_pane_ids.contains(&new_pane_id));
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(tab_id),
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

    // The first split creates `first_split_pane_id` and spawns its PTY. The
    // root pane has no PTY.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let first_split_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
    let first_split_pane_resize_count_before = fake_pty_backend
        .list_pane_sizes(first_split_pane_id)
        .unwrap()
        .len();

    // Second split creates B off the now-focused A. A must reflow even though the
    // PTY-less root is in the layout — it must not abort the resize batch.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));

    // The second split reflows A exactly once more (spawn size + this reflow).
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(first_split_pane_id)
            .unwrap()
            .len(),
        first_split_pane_resize_count_before + 1
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
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
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

    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
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

    // The first session holds the acting client.
    let first_client_id = ClientId::new();
    let first_session_id = SessionId::new();
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut first_session = build_bare_session(first_session_id);
    register_pane_record(&mut first_session, first_pane_id);
    register_session_tab(&mut first_session, first_tab_id, first_pane_id);
    attach_client(
        &mut first_session,
        first_client_id,
        first_tab_id,
        Some(first_pane_id),
    );
    runtime
        .session_by_id
        .insert(first_session_id, first_session);

    // The second session owns the target pane. Its own client views
    // `second_tab_id` through a 40x10 viewport.
    let second_client_id = ClientId::new();
    let second_session_id = SessionId::new();
    let second_tab_id = TabId::new();
    let second_pane_id = PaneId::new();
    let mut second_session = build_bare_session(second_session_id);
    register_pane_record(&mut second_session, second_pane_id);
    register_session_tab(&mut second_session, second_tab_id, second_pane_id);
    let mut viewer = Client::from_attachment(
        second_client_id,
        second_session_id,
        SystemTime::now(),
        Size {
            column_count: 40,
            row_count: 10,
        },
        None,
        second_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    viewer.update_focused_pane(second_tab_id, second_pane_id);
    second_session.attach_client(viewer);
    runtime
        .session_by_id
        .insert(second_session_id, second_session);

    // A split of `second_pane_id` from the first session's client focuses no
    // client in the second session. The second session's viewer sets the
    // size: its 40x10 viewport leaves a 40x8 pane region.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(first_client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(second_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            ..build_new_pane_args()
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["PaneCreated", "LayoutChanged", "PtyResized"])
    );

    let new_pane_id = find_other_pane_id(&runtime, second_session_id, &[second_pane_id]);
    assert!(runtime.live_pane_ids.contains(&new_pane_id));
    // The new pane takes the right half of the 40x8 region, 20x8, and its
    // content inside the border is 18x6.
    assert_eq!(
        fake_pty_backend.list_pane_sizes(new_pane_id).unwrap(),
        vec![PtySize {
            column_count: 18,
            row_count: 6
        }]
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);

    // No explicit pane: the focused (split) pane closes. The root survives and
    // inherits focus — PaneClosing + PaneRemoved + LayoutChanged + PaneFocused.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .is_none());
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .count_pane_records(),
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
            .get_focused_pane_id(tab_id),
        Some(root_pane_id)
    );
    assert!(!runtime.live_pane_ids.contains(&new_pane_id));
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);

    // Close the non-focused root explicitly: nobody's focus needs repair, so
    // PaneClosing + PaneRemoved + LayoutChanged + PtyResized(the surviving
    // split pane, now full-tab) are emitted and the client stays on the split
    // pane.
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ClosePane(ClosePaneArgs {
            pane_id: Some(root_pane_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PtyResized"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
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
            .get_focused_pane_id(tab_id),
        Some(new_pane_id)
    );
    assert!(runtime.live_pane_ids.contains(&new_pane_id));
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
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
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ClosePane(ClosePaneArgs {
            pane_id: Some(new_pane_id),
            should_force_close: true,
            should_kill_process_tree: false,
        }));
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "LayoutChanged",
            "PaneFocused"
        ])
    );

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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);

    // The default-bound close key sends `should_kill_process_tree: true`: the
    // pane gets a graceful kill of its whole process group, with the graceful
    // timeout.
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ClosePane(ClosePaneArgs {
            pane_id: Some(new_pane_id),
            should_force_close: false,
            should_kill_process_tree: true,
        }));
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "LayoutChanged",
            "PaneFocused"
        ])
    );

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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);

    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ClosePane(ClosePaneArgs {
            pane_id: Some(new_pane_id),
            should_force_close: true,
            should_kill_process_tree: true,
        }));
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "LayoutChanged",
            "PaneFocused"
        ])
    );

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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
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
        runtime.session_by_id[&session_id]
            .panes
            .count_pane_records(),
        2
    );
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id]
            .get_layout_tree()
            .list_leaf_pane_ids(),
        vec![root_pane_id, new_pane_id]
    );
    assert!(runtime.live_pane_ids.contains(&new_pane_id));
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
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
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ClosePane(ClosePaneArgs {
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
        runtime.session_by_id[&session_id]
            .panes
            .count_pane_records(),
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
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ClosePane(ClosePaneArgs {
            pane_id: Some(pane_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "TabClosed", "Quit"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    assert!(runtime.session_by_id[&session_id].tabs.is_empty());
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .count_pane_records(),
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
            .get_focused_pane_id(tab_id),
        None
    );
}

#[test]
fn close_pane_last_pane_of_a_tab_moves_viewers_to_the_nearest_tab() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let landing_client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(
        &mut session,
        landing_client_id,
        first_tab_id,
        Some(first_pane_id),
    );
    attach_client(&mut session, client_id, second_tab_id, Some(second_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Emptying `second_tab_id` closes it and moves its viewer to the surviving
    // tab: PaneClosing + PaneRemoved + TabClosed + TabFocused. The session
    // keeps running with the other tab.
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ClosePane(ClosePaneArgs {
            pane_id: Some(second_pane_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    assert_eq!(runtime.session_by_id[&session_id].tabs.len(), 1);
    assert!(runtime.session_by_id[&session_id]
        .tabs
        .contains_key(&first_tab_id));
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        first_tab_id
    );
    // Unchanged from the setup: the session is not winding down.
    assert_eq!(
        *runtime.session_by_id[&session_id].get_lifecycle(),
        SessionLifecycle::Starting
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(landing_client_id)
            .expect("landing client")
            .get_placement_revision(),
        1
    );
}

#[test]
fn close_pane_unviewed_tab_repairs_stored_focus() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let third_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_pane_record(&mut session, third_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    // `second_tab_id` holds a two-member stack with `third_pane_id` expanded.
    // The client views `first_tab_id` and keeps `third_pane_id` as its focus
    // in `second_tab_id`.
    session
        .tabs
        .get_mut(&second_tab_id)
        .unwrap()
        .update_layout(LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![second_pane_id, third_pane_id],
            1,
        )));
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    // A second client with a 60x20 terminal also views `first_tab_id`. The
    // fallback viewport is the smallest across both attached clients.
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
        first_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    additional_client.update_focused_pane(first_tab_id, first_pane_id);
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
        .update_focused_pane(second_tab_id, third_pane_id);

    // No client views `second_tab_id`: its viewport is the smallest of the
    // attached clients' viewports. The stored focus moves onto the surviving
    // member: PaneClosing + PaneRemoved + LayoutChanged + PaneFocused.
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ClosePane(ClosePaneArgs {
            pane_id: Some(third_pane_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&second_tab_id].get_layout_tree(),
        &LayoutNode::Pane(second_pane_id)
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(second_tab_id),
        Some(second_pane_id)
    );
    // The client's view never moved.
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        first_tab_id
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

    // Two splits: `first_split_pane_id` takes half the width, then the second
    // split halves `first_split_pane_id` into quarters.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let first_split_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
    let size_at_half = runtime.pty_size_by_pane_id[&first_split_pane_id];
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized",
            "PtyResized"
        ])
    );
    let second_split_pane_id =
        find_other_pane_id(&runtime, session_id, &[root_pane_id, first_split_pane_id]);
    assert_eq!(
        runtime.pty_size_by_pane_id[&first_split_pane_id],
        PtySize {
            column_count: 18,
            row_count: 20
        }
    );

    // Closing B collapses the split back to [root | A]: A reclaims exactly the
    // half-width geometry it had before B existed, and its PTY is resized to
    // it — PaneClosing + PaneRemoved + LayoutChanged + PaneFocused +
    // PtyResized(A).
    let resize_count_before_close = fake_pty_backend
        .list_pane_sizes(first_split_pane_id)
        .unwrap()
        .len();
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(first_split_pane_id)
            .unwrap()
            .len(),
        resize_count_before_close + 1
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(first_split_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        size_at_half
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&first_split_pane_id],
        size_at_half
    );
    assert!(runtime.live_pane_ids.contains(&first_split_pane_id));
    assert!(!runtime
        .pty_size_by_pane_id
        .contains_key(&second_split_pane_id));
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

    // `[root | first]`, then a split of the root itself gives
    // `[[root | second] | first]`. The right-half rect of `first_split_pane_id`
    // stays the same.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let first_split_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
    let first_split_pane_pty_size = runtime.pty_size_by_pane_id[&first_split_pane_id];
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(root_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            ..build_new_pane_args()
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&first_split_pane_id],
        first_split_pane_pty_size
    );
    let first_split_pane_resize_count = fake_pty_backend
        .list_pane_sizes(first_split_pane_id)
        .unwrap()
        .len();

    // Closing the second split pane restores `[root | first]`. The rect of
    // `first_split_pane_id` does not change, so the reflow does not resize its
    // PTY: PaneClosing + PaneRemoved + LayoutChanged + PaneFocused, and no
    // PtyResized. The root pane has no PTY.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(first_split_pane_id)
            .unwrap()
            .len(),
        first_split_pane_resize_count
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&first_split_pane_id],
        first_split_pane_pty_size
    );
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
    let split_pane_id =
        find_other_pane_id(&runtime, session_id, &[front_root_pane_id, back_pane_id]);
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(back_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            ..build_new_pane_args()
        }),
    ));
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["PaneClosing", "PaneRemoved", "LayoutChanged"])
    );

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
    let main_split_pane_id =
        find_other_pane_id(&runtime, session_id, &[main_root_pane_id, solo_pane_id]);
    let main_split_size_history_before = fake_pty_backend
        .list_pane_sizes(main_split_pane_id)
        .unwrap();

    // The solo-tab viewer closes its tab's only pane: the tab closes, and that
    // viewer moves to the main tab, whose pane region shrinks to the viewer's
    // 40x8. The live PTY reflows from 38x20 to 18x6: PaneClosing + PaneRemoved +
    // TabClosed + TabFocused + PaneFocused + PtyResized.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(solo_viewer_client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(solo_viewer_client_id)
            .unwrap()
            .get_active_tab_id(),
        main_tab_id
    );
    let tightened_pty_size = PtySize {
        column_count: 18,
        row_count: 6,
    };
    assert_eq!(
        main_split_size_history_before,
        vec![PtySize {
            column_count: 38,
            row_count: 20
        }]
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(main_split_pane_id)
            .unwrap(),
        vec![main_split_size_history_before[0], tightened_pty_size]
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&main_split_pane_id],
        tightened_pty_size
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
    let split_pane_id =
        find_other_pane_id(&runtime, session_id, &[front_root_pane_id, back_pane_id]);
    let split_pane_resize_count = fake_pty_backend
        .list_pane_sizes(split_pane_id)
        .unwrap()
        .len();
    let split_pane_pty_size = runtime.pty_size_by_pane_id[&split_pane_id];

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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "TabClosed"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert!(!runtime.session_by_id[&session_id]
        .tabs
        .contains_key(&back_tab_id));
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .len(),
        split_pane_resize_count
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        split_pane_pty_size
    );
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
    let first_split_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Stacked {
                source_pane_id: None,
                tab_id: None,
            },
            ..build_new_pane_args()
        }),
    ));
    let stacked_pane_id =
        find_other_pane_id(&runtime, session_id, &[root_pane_id, first_split_pane_id]);
    let first_split_pane_resize_count = fake_pty_backend
        .list_pane_sizes(first_split_pane_id)
        .unwrap()
        .len();
    let first_split_pane_pty_size = runtime.pty_size_by_pane_id[&first_split_pane_id];
    let stacked_pane_resize_count = fake_pty_backend
        .list_pane_sizes(stacked_pane_id)
        .unwrap()
        .len();

    // Closing the root pane hands the stack the full tab. The expanded member
    // `stacked_pane_id` reflows wider. The collapsed member
    // `first_split_pane_id` has no content rect and keeps its last size, with
    // no event: PaneClosing + PaneRemoved + LayoutChanged + PtyResized for
    // `stacked_pane_id`.
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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PtyResized"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let stacked_pane_size_history = fake_pty_backend.list_pane_sizes(stacked_pane_id).unwrap();
    assert_eq!(
        stacked_pane_size_history.len(),
        stacked_pane_resize_count + 1
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&stacked_pane_id],
        *stacked_pane_size_history.last().unwrap()
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(first_split_pane_id)
            .unwrap()
            .len(),
        first_split_pane_resize_count
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&first_split_pane_id],
        first_split_pane_pty_size
    );
}

#[test]
fn close_pane_repairs_focus_for_every_client_focused_on_it() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let first_client_id = ClientId::new();
    let second_client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, first_client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(first_client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);

    // A second client also focuses the split pane.
    let mut additional_client = Client::from_attachment(
        second_client_id,
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
        CommandSource::from_key_binding(first_client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    for client_id in [first_client_id, second_client_id] {
        assert_eq!(
            runtime.session_by_id[&session_id]
                .clients
                .get_client_by_id(client_id)
                .unwrap()
                .get_focused_pane_id(tab_id),
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let split_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);

    // Scroll both panes up, and highlight text in the one about to close.
    {
        let scrolled_client = runtime
            .session_by_id
            .get_mut(&session_id)
            .unwrap()
            .clients
            .get_client_mut_by_id(client_id)
            .unwrap();
        scrolled_client.set_scroll_offset(split_pane_id, 5);
        scrolled_client.set_scroll_offset(root_pane_id, 3);
        scrolled_client.set_selection(
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "LayoutChanged",
            "PaneFocused"
        ])
    );

    let client_after_close = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .unwrap();
    // The closed pane's scroll offset and highlight are removed. The surviving
    // pane's offset stays the same.
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
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(&mut session, client_id, second_tab_id, Some(second_pane_id));
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
        client.set_scroll_offset(second_pane_id, 5);
        client.set_scroll_offset(first_pane_id, 3);
        client.set_selection(
            second_pane_id,
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
            tab_id: Some(second_tab_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "TabClosed",
            "TabFocused",
            "PaneFocused"
        ])
    );

    let client = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .unwrap();
    // Every pane of the closed tab loses its offset and its highlight; the
    // surviving tab's pane keeps its own offset.
    assert_eq!(client.get_scroll_offset(second_pane_id), 0);
    assert_eq!(client.get_selection(second_pane_id), None);
    assert!(!client.is_view_held(second_pane_id));
    assert_eq!(client.get_scroll_offset(first_pane_id), 3);
    assert!(client.is_view_held(first_pane_id));
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
            placement: NewPanePlacement::Stacked {
                source_pane_id: None,
                tab_id: None,
            },
            ..build_new_pane_args()
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);

    // Closing the expanded stack member collapses the two-member stack back to
    // a plain leaf, and focus repairs onto the survivor.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
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
            .get_focused_pane_id(tab_id),
        Some(root_pane_id)
    );
}

#[test]
fn close_pane_with_no_attached_clients_succeeds() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, tab_id, first_pane_id);
    session
        .tabs
        .get_mut(&tab_id)
        .unwrap()
        .update_layout(LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![first_pane_id, second_pane_id],
            1,
        )));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // No client is attached anywhere, so the tab solves against the nominal
    // 80x24 viewport; with nobody's focus to repair, only PaneClosing +
    // PaneRemoved + LayoutChanged are emitted.
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ClosePane(ClosePaneArgs {
            pane_id: Some(second_pane_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }));
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(second_pane_id)
        .is_none());
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .count_pane_records(),
        1
    );
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(first_pane_id)
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
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
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ClosePane(ClosePaneArgs {
            pane_id: Some(new_pane_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }));
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "LayoutChanged",
            "PaneFocused"
        ])
    );

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
    let first_client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let surviving_pane_id = PaneId::new();
    let closing_pane_id = PaneId::new();

    let mut first_session = build_bare_session(SessionId::new());
    register_pane_record(&mut first_session, first_pane_id);
    register_session_tab(&mut first_session, first_tab_id, first_pane_id);
    attach_client(
        &mut first_session,
        first_client_id,
        first_tab_id,
        Some(first_pane_id),
    );
    let first_session_id = first_session.session_id;
    runtime
        .session_by_id
        .insert(first_session_id, first_session);

    let mut second_session = build_bare_session(SessionId::new());
    register_pane_record(&mut second_session, surviving_pane_id);
    register_pane_record(&mut second_session, closing_pane_id);
    register_session_tab(&mut second_session, second_tab_id, surviving_pane_id);
    second_session
        .tabs
        .get_mut(&second_tab_id)
        .unwrap()
        .update_layout(LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![surviving_pane_id, closing_pane_id],
            1,
        )));
    let second_session_id = second_session.session_id;
    runtime
        .session_by_id
        .insert(second_session_id, second_session);

    // An explicit pane target is global: issued by the first session's client,
    // it closes the pane in the second session and leaves the first session
    // unchanged.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(first_client_id),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(closing_pane_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    assert!(runtime.session_by_id[&second_session_id]
        .panes
        .get_pane_record_by_id(closing_pane_id)
        .is_none());
    assert_eq!(
        runtime.session_by_id[&second_session_id].tabs[&second_tab_id].get_layout_tree(),
        &LayoutNode::Pane(surviving_pane_id)
    );
    assert_eq!(
        runtime.session_by_id[&first_session_id]
            .panes
            .count_pane_records(),
        1
    );
    assert_eq!(
        runtime.session_by_id[&first_session_id].tabs[&first_tab_id].get_layout_tree(),
        &LayoutNode::Pane(first_pane_id)
    );
    assert_eq!(
        runtime.session_by_id[&first_session_id]
            .clients
            .get_client_by_id(first_client_id)
            .unwrap()
            .get_focused_pane_id(first_tab_id),
        Some(first_pane_id)
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

/// A session with one tab that one client views, whose root pane was split
/// rightward once through dispatch, so the split pane has a live PTY.
struct ResizeFixture {
    runtime: Server,
    fake_pty_backend: Arc<FakePtyBackend>,
    session_id: SessionId,
    /// The attached client that views the tab.
    client_id: ClientId,
    /// The tab's first pane, left of `split_pane_id`.
    root_pane_id: PaneId,
    /// The pane the split created.
    split_pane_id: PaneId,
    /// The PTY size `split_pane_id` spawned at.
    split_pane_pty_size: PtySize,
}

/// Build a [`ResizeFixture`]. Panics unless the split applies with
/// `PaneCreated`, `LayoutChanged`, `PaneFocused` and `PtyResized`.
fn build_resize_fixture() -> ResizeFixture {
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let split_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
    let split_pane_pty_size = runtime.pty_size_by_pane_id[&split_pane_id];
    ResizeFixture {
        runtime,
        fake_pty_backend,
        session_id,
        client_id,
        root_pane_id,
        split_pane_id,
        split_pane_pty_size,
    }
}

#[test]
fn move_pane_swaps_the_focused_pane_with_its_directional_neighbor_and_announces_the_placement() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        root_pane_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    let tab_id = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_active_tab_id();

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::MovePane(MovePaneArgs {
            pane_id: None,
            direction: Direction::Left,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PanePlacementCommitted", "LayoutChanged"]
            );
            assert_eq!(
                emitted_events[0],
                Event::PanePlacementCommitted(PanePlacementCommitted {
                    command_id,
                    source_pane_id: split_pane_id,
                    source_tab_id: Some(tab_id),
                    destination_tab_id: Some(tab_id),
                    placement_target: PanePlacementTarget::Swap {
                        target_pane_id: root_pane_id,
                    },
                })
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree().list_leaf_pane_ids(),
        vec![split_pane_id, root_pane_id]
    );
}

#[test]
fn move_pane_without_a_neighbor_is_rejected_without_changing_the_layout() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        root_pane_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    let session_records_before = serialize_session_records(&runtime);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::MovePane(MovePaneArgs {
            pane_id: None,
            direction: Direction::Right,
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
    assert_eq!(serialize_session_records(&runtime), session_records_before);
    let session = &runtime.session_by_id[&session_id];
    let tab_id = session
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_active_tab_id();
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree().list_leaf_pane_ids(),
        vec![root_pane_id, split_pane_id]
    );
}

#[test]
fn a_same_tab_swap_placement_exchanges_pane_occupants() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        root_pane_id,
        split_pane_id,
        ..
    } = build_resize_fixture();

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: root_pane_id,
            placement_target: PanePlacementTarget::Swap {
                target_pane_id: split_pane_id,
            },
            expected_placement_revision: None,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PanePlacementCommitted", "LayoutChanged"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let session = &runtime.session_by_id[&session_id];
    let tab_id = session
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_active_tab_id();
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree().list_leaf_pane_ids(),
        vec![split_pane_id, root_pane_id]
    );
}

#[test]
fn a_confirmed_same_tab_swap_advances_session_and_client_placement_revisions() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        root_pane_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    let session_revision_before = runtime.session_by_id[&session_id].get_placement_revision();
    let client_revision_before = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_placement_revision();

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: root_pane_id,
            placement_target: PanePlacementTarget::Swap {
                target_pane_id: split_pane_id,
            },
            expected_placement_revision: Some(PlacementRevision {
                session_revision: session_revision_before,
                client_revision: client_revision_before,
            }),
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: applied_command_id,
            emitted_events,
        } => {
            assert_eq!(applied_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PanePlacementCommitted", "LayoutChanged"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session.get_placement_revision(),
        session_revision_before + 1
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_placement_revision(),
        client_revision_before + 1
    );
}

#[test]
fn swapping_a_pane_with_itself_is_a_no_op() {
    let ResizeFixture {
        mut runtime,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    let session_records_before = serialize_session_records(&runtime);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: split_pane_id,
            placement_target: PanePlacementTarget::Swap {
                target_pane_id: split_pane_id,
            },
            expected_placement_revision: None,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert!(emitted_events.is_empty());
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(serialize_session_records(&runtime), session_records_before);
}

#[test]
fn swapping_panes_in_different_tabs_exchanges_slots_without_lifecycle_events() {
    let CommandMatrixFixture {
        mut runtime,
        client_id,
        first_tab_id,
        first_pane_id,
        session_id,
        second_tab_id,
        second_pane_id,
    } = build_command_matrix_server(ClientOrigin::Local);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: first_pane_id,
            placement_target: PanePlacementTarget::Swap {
                target_pane_id: second_pane_id,
            },
            expected_placement_revision: None,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                [
                    "PanePlacementCommitted",
                    "LayoutChanged",
                    "LayoutChanged",
                    "TabFocused",
                    "PaneFocused",
                    "PaneFocused",
                ]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session.tabs[&first_tab_id]
            .get_layout_tree()
            .list_leaf_pane_ids(),
        vec![second_pane_id]
    );
    assert_eq!(
        session.tabs[&second_tab_id]
            .get_layout_tree()
            .list_leaf_pane_ids(),
        vec![first_pane_id]
    );
    for pane_id in [first_pane_id, second_pane_id] {
        assert_eq!(
            session
                .panes
                .get_pane_record_by_id(pane_id)
                .map(PaneRecord::get_lifecycle),
            Some(&PaneLifecycle::Spawning)
        );
    }
    let client = session.clients.get_client_by_id(client_id).expect("client");
    assert_eq!(client.get_active_tab_id(), second_tab_id);
    assert_eq!(
        client.get_focused_pane_id(second_tab_id),
        Some(first_pane_id)
    );
    assert_eq!(session.get_placement_revision(), 1);
    assert_eq!(client.get_placement_revision(), 1);
    assert_eq!(session.tabs.len(), 2);
}

#[test]
fn swapping_a_sole_source_pane_reflows_source_viewers_without_closing_source_tab() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        client_id: acting_client_id,
        root_pane_id: source_root_pane_id,
        split_pane_id: moved_pane_id,
        ..
    } = build_resize_fixture();
    let source_tab_id = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(acting_client_id)
        .expect("acting client")
        .get_active_tab_id();

    assert_eq!(
        get_command_outcome(&runtime.dispatch(build_command_envelope(
            CommandSource::from_key_binding(acting_client_id),
            Command::ClosePane(ClosePaneArgs {
                pane_id: Some(moved_pane_id),
                should_force_close: true,
                should_kill_process_tree: false,
            }),
        ))),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "LayoutChanged",
            "PaneFocused"
        ])
    );

    assert_eq!(
        get_command_outcome(&runtime.dispatch(build_command_envelope(
            CommandSource::from_key_binding(acting_client_id),
            Command::NewTab(NewTabArgs::default()),
        ))),
        Ok(vec![
            "TabCreated",
            "PaneCreated",
            "TabFocused",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let destination_tab_id = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(acting_client_id)
        .expect("acting client")
        .get_active_tab_id();
    let destination_pane_id = runtime.session_by_id[&session_id].tabs[&destination_tab_id]
        .get_layout_tree()
        .list_leaf_pane_ids()[0];
    let destination_pane_sizes_before = fake_pty_backend
        .list_pane_sizes(destination_pane_id)
        .expect("destination pane was spawned");

    {
        let session = runtime.session_by_id.get_mut(&session_id).expect("session");
        let acting_client = session
            .clients
            .get_client_mut_by_id(acting_client_id)
            .expect("acting client");
        acting_client.update_active_tab_id(source_tab_id);
        acting_client.update_focused_pane(source_tab_id, source_root_pane_id);
        session.attach_client({
            let mut source_viewer = Client::from_attachment(
                ClientId::new(),
                session_id,
                SystemTime::now(),
                Size {
                    column_count: 40,
                    row_count: 10,
                },
                Some(PaneArea::Reported(Size {
                    column_count: 40,
                    row_count: 10,
                })),
                source_tab_id,
                ClientOrigin::Local,
                "C-source-viewer".to_string(),
                0,
            );
            source_viewer.update_focused_pane(source_tab_id, source_root_pane_id);
            source_viewer
        });
    }

    let source_viewer_client_id = runtime.session_by_id[&session_id]
        .clients
        .list_attached_clients()
        .map(Client::get_client_id)
        .find(|client_id| *client_id != acting_client_id)
        .expect("source viewer");

    let emitted_events = match runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(acting_client_id),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: source_root_pane_id,
            placement_target: PanePlacementTarget::Swap {
                target_pane_id: destination_pane_id,
            },
            expected_placement_revision: None,
        }),
    )) {
        CommandResult::Ok { emitted_events, .. } => emitted_events,
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    };

    assert_eq!(
        list_event_names(&emitted_events),
        [
            "PanePlacementCommitted",
            "LayoutChanged",
            "LayoutChanged",
            "TabFocused",
            "PaneFocused",
            "PaneFocused",
            "PaneFocused",
            "PtyResized"
        ]
    );
    // The source viewer's 40x10 pane area leaves 38x8 inside the border.
    assert_eq!(
        emitted_events[7],
        Event::PtyResized(PtyResized {
            pane_id: destination_pane_id,
            pty_size: PtySize {
                column_count: 38,
                row_count: 8
            },
        })
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(destination_pane_id)
            .expect("destination pane remains live")
            .len(),
        destination_pane_sizes_before.len() + 1
    );

    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs.len(), 2);
    assert_eq!(
        session.tabs[&source_tab_id]
            .get_layout_tree()
            .list_leaf_pane_ids(),
        vec![destination_pane_id]
    );
    assert_eq!(
        session.tabs[&destination_tab_id]
            .get_layout_tree()
            .list_leaf_pane_ids(),
        vec![source_root_pane_id]
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(source_viewer_client_id)
            .expect("source viewer")
            .get_active_tab_id(),
        source_tab_id
    );
}

#[test]
fn placing_a_sole_pane_in_another_tab_closes_only_the_empty_source_tab() {
    let CommandMatrixFixture {
        mut runtime,
        client_id,
        first_tab_id,
        first_pane_id,
        session_id,
        second_tab_id,
        second_pane_id,
    } = build_command_matrix_server(ClientOrigin::Local);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: first_pane_id,
            placement_target: PanePlacementTarget::Split {
                destination_tab_id: second_tab_id,
                anchor: PanePlacementAnchor::Tab,
                direction: Direction::Right,
            },
            expected_placement_revision: None,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                [
                    "PanePlacementCommitted",
                    "LayoutChanged",
                    "TabFocused",
                    "PaneFocused",
                    "TabClosed"
                ]
            );
            assert_eq!(
                emitted_events[0],
                Event::PanePlacementCommitted(PanePlacementCommitted {
                    command_id,
                    source_pane_id: first_pane_id,
                    source_tab_id: Some(first_tab_id),
                    destination_tab_id: Some(second_tab_id),
                    placement_target: PanePlacementTarget::Split {
                        destination_tab_id: second_tab_id,
                        anchor: PanePlacementAnchor::Tab,
                        direction: Direction::Right,
                    },
                })
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let session = &runtime.session_by_id[&session_id];
    assert!(!session.tabs.contains_key(&first_tab_id));
    assert_eq!(
        session.tabs[&second_tab_id]
            .get_layout_tree()
            .list_leaf_pane_ids(),
        vec![second_pane_id, first_pane_id]
    );
    assert_eq!(session.panes.count_pane_records(), 2);
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(first_pane_id)
            .map(PaneRecord::get_lifecycle),
        Some(&PaneLifecycle::Spawning)
    );
    let client = session.clients.get_client_by_id(client_id).expect("client");
    assert_eq!(client.get_active_tab_id(), second_tab_id);
    assert_eq!(
        client.get_focused_pane_id(second_tab_id),
        Some(first_pane_id)
    );
    assert_eq!(session.get_placement_revision(), 1);
    assert_eq!(client.get_placement_revision(), 1);
}

#[test]
fn placing_a_sole_pane_reflows_the_tab_where_source_viewers_land() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let source_tab_id = TabId::new();
    let landing_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    let landing_root_pane_id = PaneId::new();
    let destination_root_pane_id = PaneId::new();
    let acting_client_id = ClientId::new();
    let source_viewer_client_id = ClientId::new();
    let landing_viewer_client_id = ClientId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, source_pane_id);
    register_pane_record(&mut session, landing_root_pane_id);
    register_pane_record(&mut session, destination_root_pane_id);
    register_session_tab(&mut session, source_tab_id, source_pane_id);
    register_session_tab(&mut session, landing_tab_id, landing_root_pane_id);
    register_session_tab(&mut session, destination_tab_id, destination_root_pane_id);
    attach_client_with_reported_pane_area(
        &mut session,
        acting_client_id,
        source_tab_id,
        Some(source_pane_id),
        Some(PaneArea::Reported(Size {
            column_count: 80,
            row_count: 24,
        })),
    );
    attach_client_with_reported_pane_area(
        &mut session,
        source_viewer_client_id,
        source_tab_id,
        Some(source_pane_id),
        Some(PaneArea::Reported(Size {
            column_count: 40,
            row_count: 10,
        })),
    );
    attach_client_with_reported_pane_area(
        &mut session,
        landing_viewer_client_id,
        landing_tab_id,
        Some(landing_root_pane_id),
        Some(PaneArea::Reported(Size {
            column_count: 80,
            row_count: 24,
        })),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    assert_eq!(
        get_command_outcome(&runtime.dispatch(build_command_envelope(
            CommandSource::from_key_binding(landing_viewer_client_id),
            Command::NewPane(build_new_pane_args()),
        ))),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let landing_split_pane_id = find_other_pane_id(
        &runtime,
        session_id,
        &[
            source_pane_id,
            landing_root_pane_id,
            destination_root_pane_id,
        ],
    );
    let landing_split_sizes_before = fake_pty_backend
        .list_pane_sizes(landing_split_pane_id)
        .expect("landing split pane spawned");
    let landing_client_revision_before = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(landing_viewer_client_id)
        .expect("landing viewer")
        .get_placement_revision();

    let placement_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(acting_client_id),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id,
            placement_target: PanePlacementTarget::Split {
                destination_tab_id,
                anchor: PanePlacementAnchor::Tab,
                direction: Direction::Right,
            },
            expected_placement_revision: None,
        }),
    ));
    match placement_result {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(
                list_event_names(&emitted_events),
                [
                    "PanePlacementCommitted",
                    "LayoutChanged",
                    "TabFocused",
                    "PaneFocused",
                    "TabClosed",
                    "TabFocused",
                    "PaneFocused",
                    "PtyResized",
                ]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let session = &runtime.session_by_id[&session_id];
    assert!(!session.tabs.contains_key(&source_tab_id));
    assert_eq!(
        session.tabs[&landing_tab_id]
            .get_layout_tree()
            .list_leaf_pane_ids(),
        vec![landing_root_pane_id, landing_split_pane_id]
    );
    assert_eq!(
        session.tabs[&destination_tab_id]
            .get_layout_tree()
            .list_leaf_pane_ids(),
        vec![destination_root_pane_id, source_pane_id]
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(landing_split_pane_id)
            .expect("landing split pane remains live"),
        vec![
            landing_split_sizes_before[0],
            PtySize {
                column_count: 18,
                row_count: 8,
            },
        ]
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(source_viewer_client_id)
            .expect("source viewer")
            .get_active_tab_id(),
        landing_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(landing_viewer_client_id)
            .expect("landing viewer")
            .get_placement_revision(),
        landing_client_revision_before + 1
    );
}

#[test]
fn placement_uses_source_viewer_size_when_source_viewers_land_in_destination() {
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    let destination_pane_id = PaneId::new();
    let acting_client_id = ClientId::new();
    let source_viewer_client_id = ClientId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, source_pane_id);
    register_pane_record(&mut session, destination_pane_id);
    register_session_tab(&mut session, source_tab_id, source_pane_id);
    register_session_tab(&mut session, destination_tab_id, destination_pane_id);
    attach_client_with_reported_pane_area(
        &mut session,
        acting_client_id,
        source_tab_id,
        Some(source_pane_id),
        Some(PaneArea::Reported(Size {
            column_count: 80,
            row_count: 24,
        })),
    );
    attach_client_with_reported_pane_area(
        &mut session,
        source_viewer_client_id,
        source_tab_id,
        Some(source_pane_id),
        Some(PaneArea::Reported(Size {
            column_count: 40,
            row_count: 10,
        })),
    );

    let destination_tab_size = super::pane::compute_placement_destination_tab_size(
        &session,
        source_tab_id,
        destination_tab_id,
        Some(destination_tab_id),
        acting_client_id,
    );
    match destination_tab_size {
        Ok(viewport_size) => assert_eq!(
            viewport_size,
            Size {
                column_count: 40,
                row_count: 10,
            }
        ),
        Err(_) => panic!("destination has no drawable source viewer"),
    }
}

#[test]
fn place_pane_with_an_unknown_destination_anchor_rejects_without_mutating_session_state() {
    let CommandMatrixFixture {
        mut runtime,
        client_id,
        first_pane_id,
        second_tab_id,
        ..
    } = build_command_matrix_server(ClientOrigin::Local);
    let session_records_before = serialize_session_records(&runtime);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: first_pane_id,
            placement_target: PanePlacementTarget::Split {
                destination_tab_id: second_tab_id,
                anchor: PanePlacementAnchor::Pane(PaneId::new()),
                direction: Direction::Right,
            },
            expected_placement_revision: None,
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
    assert_eq!(serialize_session_records(&runtime), session_records_before);
}

#[test]
fn place_pane_refuses_when_the_session_revision_cannot_advance() {
    let CommandMatrixFixture {
        mut runtime,
        client_id,
        first_pane_id,
        session_id,
        second_tab_id,
        ..
    } = build_command_matrix_server(ClientOrigin::Local);
    replace_persisted_placement_revisions(&mut runtime, session_id, client_id, u64::MAX, 0);
    let session_records_before = serialize_session_records(&runtime);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: first_pane_id,
            placement_target: PanePlacementTarget::Split {
                destination_tab_id: second_tab_id,
                anchor: PanePlacementAnchor::Tab,
                direction: Direction::Right,
            },
            expected_placement_revision: None,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("placement revision cannot advance".to_string()),
        }
    );
    assert_eq!(serialize_session_records(&runtime), session_records_before);
}

#[test]
fn place_pane_refuses_when_an_affected_client_revision_cannot_advance() {
    let CommandMatrixFixture {
        mut runtime,
        client_id,
        first_pane_id,
        session_id,
        second_tab_id,
        ..
    } = build_command_matrix_server(ClientOrigin::Local);
    replace_persisted_placement_revisions(&mut runtime, session_id, client_id, 0, u64::MAX);
    let session_records_before = serialize_session_records(&runtime);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: first_pane_id,
            placement_target: PanePlacementTarget::Split {
                destination_tab_id: second_tab_id,
                anchor: PanePlacementAnchor::Tab,
                direction: Direction::Right,
            },
            expected_placement_revision: None,
        }),
    );
    let command_id = command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("placement revision cannot advance".to_string()),
        }
    );
    assert_eq!(serialize_session_records(&runtime), session_records_before);
}

#[test]
fn a_stale_place_pane_confirmation_is_rejected_without_mutating_session_state() {
    let CommandMatrixFixture {
        mut runtime,
        client_id,
        first_pane_id,
        second_pane_id,
        ..
    } = build_command_matrix_server(ClientOrigin::Local);
    let expected_placement_revision = Some(PlacementRevision {
        session_revision: 0,
        client_revision: 0,
    });

    let first_command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: first_pane_id,
            placement_target: PanePlacementTarget::Swap {
                target_pane_id: second_pane_id,
            },
            expected_placement_revision,
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(first_command_envelope)),
        Ok(vec![
            "PanePlacementCommitted",
            "LayoutChanged",
            "LayoutChanged",
            "TabFocused",
            "PaneFocused",
            "PaneFocused"
        ])
    );
    let session_records_before_retry = serialize_session_records(&runtime);
    let rendered_at = Instant::now();
    assert!(runtime.render_scheduler.claim_due_render(rendered_at));

    let retry_command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: first_pane_id,
            placement_target: PanePlacementTarget::Swap {
                target_pane_id: second_pane_id,
            },
            expected_placement_revision,
        }),
    );
    let command_id = retry_command_envelope.command_id;
    assert_eq!(
        runtime.dispatch(retry_command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("placement preview is stale; refresh and confirm again".to_string()),
        }
    );
    assert_eq!(
        serialize_session_records(&runtime),
        session_records_before_retry
    );
    assert_eq!(
        runtime.render_scheduler.compute_next_wakeup(rendered_at),
        Some(FRAME_INTERVAL_DURATION)
    );
}

#[test]
fn placement_commits_model_state_when_a_destination_pty_resize_fails() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        client_id,
        root_pane_id: source_root_pane_id,
        split_pane_id: moved_pane_id,
        ..
    } = build_resize_fixture();
    let source_tab_id = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_active_tab_id();

    assert_eq!(
        get_command_outcome(&runtime.dispatch(build_command_envelope(
            CommandSource::from_key_binding(client_id),
            Command::NewTab(NewTabArgs::default()),
        ))),
        Ok(vec![
            "TabCreated",
            "PaneCreated",
            "TabFocused",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let destination_tab_id = find_other_tab_id(&runtime, session_id, source_tab_id);
    let destination_pane_id = runtime.session_by_id[&session_id].tabs[&destination_tab_id]
        .get_layout_tree()
        .list_leaf_pane_ids()[0];
    let moved_pane_size_before = runtime.pty_size_by_pane_id[&moved_pane_id];
    let moved_pane_history_before = fake_pty_backend
        .list_pane_sizes(moved_pane_id)
        .expect("moved pane was spawned");
    let destination_pane_history_before = fake_pty_backend
        .list_pane_sizes(destination_pane_id)
        .expect("destination pane was spawned");
    fake_pty_backend.fail_resizes_on(
        moved_pane_id,
        PtyError::Io {
            detail: "resize refused".to_string(),
        },
    );

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: moved_pane_id,
            placement_target: PanePlacementTarget::Split {
                destination_tab_id,
                anchor: PanePlacementAnchor::Tab,
                direction: Direction::Right,
            },
            expected_placement_revision: None,
        }),
    );
    let emitted_events = match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => emitted_events,
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    };

    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&source_tab_id]
            .get_layout_tree()
            .list_leaf_pane_ids(),
        vec![source_root_pane_id]
    );
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&destination_tab_id]
            .get_layout_tree()
            .list_leaf_pane_ids(),
        vec![destination_pane_id, moved_pane_id]
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(moved_pane_id)
            .map(PaneRecord::get_lifecycle),
        Some(&PaneLifecycle::Running)
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(moved_pane_id).unwrap(),
        moved_pane_history_before
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&moved_pane_id],
        moved_pane_size_before
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(destination_pane_id)
            .unwrap()
            .len(),
        destination_pane_history_before.len() + 1
    );
    assert_eq!(
        list_event_names(&emitted_events),
        [
            "PanePlacementCommitted",
            "LayoutChanged",
            "LayoutChanged",
            "PaneFocused",
            "PaneFocused",
            "PtyResized"
        ]
    );
    // The destination pane takes the left half of the 80x22 pane region; the
    // refused resize of the moved pane emits nothing.
    assert_eq!(
        emitted_events[5],
        Event::PtyResized(PtyResized {
            pane_id: destination_pane_id,
            pty_size: PtySize {
                column_count: 38,
                row_count: 20
            },
        })
    );
}

#[test]
fn scroll_pane_uses_the_named_clients_view_and_signed_lines() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    let second_client_id = ClientId::new();
    let tab_id = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_active_tab_id();
    attach_client(
        runtime.session_by_id.get_mut(&session_id).expect("session"),
        second_client_id,
        tab_id,
        Some(split_pane_id),
    );
    runtime.handle_pty_output(split_pane_id, &b"\n".repeat(200));

    let scroll_up_command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), Some(second_client_id)),
        Command::ScrollPane(ScrollPaneArgs {
            pane_id: Some(split_pane_id),
            scroll_line_count: 3,
        }),
    );
    match runtime.dispatch(scroll_up_command_envelope) {
        CommandResult::Ok { emitted_events, .. } => assert!(emitted_events.is_empty()),
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        get_client_scroll_offset(&runtime, client_id, split_pane_id),
        0
    );
    assert_eq!(
        get_client_scroll_offset(&runtime, second_client_id, split_pane_id),
        3
    );

    let scroll_down_command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), Some(second_client_id)),
        Command::ScrollPane(ScrollPaneArgs {
            pane_id: Some(split_pane_id),
            scroll_line_count: -2,
        }),
    );
    match runtime.dispatch(scroll_down_command_envelope) {
        CommandResult::Ok { emitted_events, .. } => assert!(emitted_events.is_empty()),
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        get_client_scroll_offset(&runtime, second_client_id, split_pane_id),
        1
    );
}

#[test]
fn scroll_pane_with_zero_lines_keeps_the_view_unchanged() {
    let ResizeFixture {
        mut runtime,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    runtime.handle_pty_output(split_pane_id, &b"\n".repeat(200));
    runtime.scroll_up(client_id, split_pane_id, 4);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ScrollPane(ScrollPaneArgs {
            pane_id: Some(split_pane_id),
            scroll_line_count: 0,
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => assert!(emitted_events.is_empty()),
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        get_client_scroll_offset(&runtime, client_id, split_pane_id),
        4
    );
}

#[test]
fn scroll_pane_accepts_the_minimum_negative_line_count() {
    let ResizeFixture {
        mut runtime,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    runtime.handle_pty_output(split_pane_id, &b"\n".repeat(200));
    runtime.scroll_up(client_id, split_pane_id, 4);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ScrollPane(ScrollPaneArgs {
            pane_id: Some(split_pane_id),
            scroll_line_count: i32::MIN,
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => assert!(emitted_events.is_empty()),
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        get_client_scroll_offset(&runtime, client_id, split_pane_id),
        0
    );
}

#[test]
fn resize_pane_grows_the_focused_pane_and_reflows_its_pty() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        client_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();

    // The client focuses `split_pane_id`. Growing its left border by 5 takes 5
    // columns from the root pane. The root pane has no PTY: one PtyResized
    // follows the LayoutChanged.
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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PtyResized"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let expected_pty_size = PtySize {
        column_count: split_pane_pty_size.column_count + 5,
        row_count: split_pane_pty_size.row_count,
    };
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        expected_pty_size
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        expected_pty_size
    );
}

#[test]
fn resize_pane_negative_size_shrinks_the_focused_pane() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        client_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();

    // The client focuses `split_pane_id`. A negative amount moves its left
    // border inward: `split_pane_id` gives 5 columns to the root pane.
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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PtyResized"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let expected_pty_size = PtySize {
        column_count: split_pane_pty_size.column_count - 5,
        row_count: split_pane_pty_size.row_count,
    };
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        expected_pty_size
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        expected_pty_size
    );
}

#[test]
fn resize_pane_via_in_session_cli_defaults_to_the_issuing_pane() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        root_pane_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();

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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged", "PtyResized"])
    );

    let expected_pty_size = PtySize {
        column_count: split_pane_pty_size.column_count - 3,
        row_count: split_pane_pty_size.row_count,
    };
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        expected_pty_size
    );
}

#[test]
fn resize_pane_explicit_target_resolves_its_owning_session() {
    let ResizeFixture {
        mut runtime,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();

    // A CLI source naming no session and no client: the explicit pane target
    // alone finds the owning session.
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ResizePane(ResizePaneArgs {
            pane_id: Some(split_pane_id),
            direction: Direction::Left,
            resize_amount_cells: 2,
        }));
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged", "PtyResized"])
    );

    let expected_pty_size = PtySize {
        column_count: split_pane_pty_size.column_count + 2,
        row_count: split_pane_pty_size.row_count,
    };
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        expected_pty_size
    );
}

#[test]
fn resize_pane_min_size_rejection_reports_the_spare_and_mutates_nothing() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        client_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();
    let tab_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let rects_before = Server::compute_tab_content_rects(
        &runtime.session_by_id[&session_id],
        get_only_tab_id(&runtime, session_id),
        tab_size,
        PaneSizing::default(),
    );
    let resize_count_before_rejection = fake_pty_backend
        .list_pane_sizes(split_pane_id)
        .unwrap()
        .len();

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
            reason: RejectReason::MinimumSize,
            help: Some("the donating pane has only 36 spare cells to give".to_string()),
        }
    );

    let rects_after = Server::compute_tab_content_rects(
        &runtime.session_by_id[&session_id],
        get_only_tab_id(&runtime, session_id),
        tab_size,
        PaneSizing::default(),
    );
    assert_eq!(rects_after, rects_before);
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .len(),
        resize_count_before_rejection
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        split_pane_pty_size
    );
}

#[test]
fn dispatch_reporting_spare_hands_back_the_donors_spare_cells() {
    let ResizeFixture {
        mut runtime,
        client_id,
        ..
    } = build_resize_fixture();

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
                reason: RejectReason::MinimumSize,
                help: Some("the donating pane has only 36 spare cells to give".to_string()),
            },
            Some(36)
        )
    );
}

#[test]
fn dispatch_reporting_spare_reports_no_spare_for_an_applied_resize() {
    let ResizeFixture {
        mut runtime,
        client_id,
        ..
    } = build_resize_fixture();

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
    let (command_result, spare_cell_count) = runtime.dispatch_reporting_spare(command_envelope);
    match command_result {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PtyResized"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(spare_cell_count, None);
}

#[test]
fn a_border_refused_at_the_pane_minimum_writes_no_log_line() {
    let ResizeFixture {
        mut runtime,
        client_id,
        ..
    } = build_resize_fixture();
    let (_subscriber_guard, captured_logs) = koshi_observability::logging::with_test_writer();

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
            reason: RejectReason::MinimumSize,
            help: Some("the donating pane has only 36 spare cells to give".to_string()),
        }
    );
    assert_eq!(captured_logs.contents(), "");
}

#[test]
fn resize_pane_at_the_tab_edge_moves_the_opposite_border_instead() {
    let ResizeFixture {
        mut runtime,
        client_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();

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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged", "PtyResized"])
    );
    let expected_pty_size = PtySize {
        column_count: split_pane_pty_size.column_count - 1,
        row_count: split_pane_pty_size.row_count,
    };
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        expected_pty_size
    );
}

#[test]
fn resize_pane_negative_size_at_the_edge_grows_via_the_opposite_border() {
    let ResizeFixture {
        mut runtime,
        client_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();

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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged", "PtyResized"])
    );
    let expected_pty_size = PtySize {
        column_count: split_pane_pty_size.column_count + 1,
        row_count: split_pane_pty_size.row_count,
    };
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        expected_pty_size
    );
}

#[test]
fn resize_pane_with_no_border_on_the_axis_is_rejected() {
    let ResizeFixture {
        mut runtime,
        client_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();

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
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        split_pane_pty_size
    );
}

#[test]
fn resize_pane_size_zero_is_rejected() {
    let ResizeFixture {
        mut runtime,
        client_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();

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
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        split_pane_pty_size
    );
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
    let tab_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let rects_before = Server::compute_tab_content_rects(
        &runtime.session_by_id[&session_id],
        tab_id,
        tab_size,
        PaneSizing::default(),
    );

    // No client is attached anywhere, so no tab is viewed and no terminal
    // displays the result.
    let command_envelope =
        build_sessionless_cli_command_envelope(Command::ResizePane(ResizePaneArgs {
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
        Server::compute_tab_content_rects(
            &runtime.session_by_id[&session_id],
            tab_id,
            tab_size,
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
    let tab_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let rects_before = Server::compute_tab_content_rects(
        &runtime.session_by_id[&session_id],
        back_tab_id,
        tab_size,
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
        Server::compute_tab_content_rects(
            &runtime.session_by_id[&session_id],
            back_tab_id,
            tab_size,
            PaneSizing::default()
        ),
        rects_before
    );
}

#[test]
fn resize_pane_in_a_nested_split_moves_the_enclosing_border() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        root_pane_id,
        split_pane_id,
        ..
    } = build_resize_fixture();

    // A split of the root pane gives `[[root | inner] | split]`.
    // `inner_split_pane_id` touches the inner split's right edge: growing it
    // rightward moves the outer border. The inner split takes 4 columns from
    // `split_pane_id`, and `inner_split_pane_id` grows by 2.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(root_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            ..build_new_pane_args()
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let inner_split_pane_id =
        find_other_pane_id(&runtime, session_id, &[root_pane_id, split_pane_id]);
    let split_pane_pty_size_before = runtime.pty_size_by_pane_id[&split_pane_id];
    let inner_split_pane_pty_size_before = runtime.pty_size_by_pane_id[&inner_split_pane_id];

    // The client's focus followed the fresh split to `inner_split_pane_id`.
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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            // LayoutChanged, then PtyResized for `split_pane_id` and
            // `inner_split_pane_id`. The root pane has no PTY.
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PtyResized", "PtyResized"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        PtySize {
            column_count: split_pane_pty_size_before.column_count - 4,
            row_count: split_pane_pty_size_before.row_count,
        }
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&inner_split_pane_id],
        PtySize {
            column_count: inner_split_pane_pty_size_before.column_count + 2,
            row_count: inner_split_pane_pty_size_before.row_count,
        }
    );
}

// --- NewTab handler ----------------------------------------------------------

#[test]
fn new_tab_spawns_creates_and_focuses_for_the_issuer() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs.len(), 2);
    let new_tab = &session.tabs[&find_other_tab_id(&runtime, session_id, first_tab_id)];
    assert!(
        new_tab.get_tab_name().starts_with("T-"),
        "generated tab name, got {}",
        new_tab.get_tab_name()
    );
    assert_eq!(new_tab.get_tab_index(), 1);
    let new_pane_id = new_tab.get_layout_tree().list_leaf_pane_ids()[0];

    // The issuer switched onto the new tab and focuses its root pane.
    let client = session.clients.get_client_by_id(client_id).unwrap();
    assert_eq!(client.get_active_tab_id(), new_tab.get_tab_id());
    assert_eq!(
        client.get_focused_pane_id(new_tab.get_tab_id()),
        Some(new_pane_id)
    );

    // Root pane runs default shell in 80x22 middle region -> 78x20 content.
    let pane_record = session.panes.get_pane_record_by_id(new_pane_id).unwrap();
    assert_eq!(*pane_record.get_lifecycle(), PaneLifecycle::Running);
    assert_eq!(pane_record.spawn_spec, None);
    assert!(runtime.live_pane_ids.contains(&new_pane_id));
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
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    ));

    // The root pane's spec is the default shell plus the identity vars naming
    // this session, the issuing client, and the root pane itself.
    let session = &runtime.session_by_id[&session_id];
    let new_tab = &session.tabs[&find_other_tab_id(&runtime, session_id, first_tab_id)];
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
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    ));
    assert_eq!(
        get_command_outcome(&command_result),
        Ok(vec![
            "TabCreated",
            "PaneCreated",
            "TabFocused",
            "PaneFocused",
            "PtyResized"
        ])
    );

    // The generated name is `T-<adjective>-<noun>` and does not collide with
    // the existing tab.
    let session = &runtime.session_by_id[&session_id];
    let new_tab = &session.tabs[&find_other_tab_id(&runtime, session_id, first_tab_id)];
    let tab_name_parts: Vec<&str> = new_tab.get_tab_name().split('-').collect();
    assert_eq!(tab_name_parts.len(), 3, "{}", new_tab.get_tab_name());
    assert_eq!(tab_name_parts[0], "T");
    assert!(!tab_name_parts[1].is_empty() && !tab_name_parts[2].is_empty());
}

/// With every plain tab name taken, the generated name is a plain name with
/// the wrap number `-2`.
#[test]
fn generate_tab_name_skips_every_name_a_tab_of_the_session_holds() {
    // A walk that counts every plain name as taken asks about each plain tab
    // name once before it reaches a `-2` name.
    let plain_tab_names = RefCell::new(Vec::new());
    let _ = generate_name(NameKind::Tab, |candidate_tab_name| {
        let is_plain_tab_name = !candidate_tab_name.ends_with("-2");
        if is_plain_tab_name {
            plain_tab_names
                .borrow_mut()
                .push(candidate_tab_name.to_owned());
        }
        is_plain_tab_name
    });
    let plain_tab_names = plain_tab_names.into_inner();
    let mut session = build_bare_session(SessionId::new());
    for (tab_index, plain_tab_name) in plain_tab_names.iter().enumerate() {
        let tab_id = TabId::new();
        session.tabs.insert(
            tab_id,
            Tab::from_root_pane(tab_id, plain_tab_name.clone(), tab_index, PaneId::new()),
        );
    }

    let generated_tab_name = generate_tab_name(&session);

    let generated_plain_tab_name = generated_tab_name
        .strip_suffix("-2")
        .expect("every plain tab name is taken");
    assert!(plain_tab_names
        .iter()
        .any(|plain_tab_name| plain_tab_name == generated_plain_tab_name));
}

#[test]
fn new_tab_spawn_failure_commits_nothing() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    fake_pty_backend.fail_spawns_with(PtyError::Spawn {
        detail: "boom".to_string(),
    });
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
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

    // Nothing was committed: no tab, no pane record, no view moved, no handle.
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs.len(), 1);
    assert_eq!(session.panes.count_pane_records(), 1);
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        first_tab_id
    );
    assert!(runtime.live_pane_ids.is_empty());
}

#[test]
fn new_tab_explicit_client_wins_over_the_issuer() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let issuer_client_id = ClientId::new();
    let named_client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    attach_client(
        &mut session,
        issuer_client_id,
        first_tab_id,
        Some(first_pane_id),
    );
    attach_client(
        &mut session,
        named_client_id,
        first_tab_id,
        Some(first_pane_id),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(issuer_client_id),
        Command::NewTab(NewTabArgs {
            client_id: Some(named_client_id),
            ..NewTabArgs::default()
        }),
    ));
    assert_eq!(
        get_command_outcome(&command_result),
        Ok(vec![
            "TabCreated",
            "PaneCreated",
            "TabFocused",
            "PaneFocused",
            "PtyResized"
        ])
    );

    let session = &runtime.session_by_id[&session_id];
    let new_tab_id = find_other_tab_id(&runtime, session_id, first_tab_id);
    assert_eq!(
        session
            .clients
            .get_client_by_id(named_client_id)
            .unwrap()
            .get_active_tab_id(),
        new_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(issuer_client_id)
            .unwrap()
            .get_active_tab_id(),
        first_tab_id
    );
}

#[test]
fn new_tab_with_an_unattached_explicit_client_is_rejected() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
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
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        Command::NewTab(NewTabArgs::default()),
    ));
    assert_eq!(
        get_command_outcome(&command_result),
        Ok(vec![
            "TabCreated",
            "PaneCreated",
            "TabFocused",
            "PaneFocused",
            "PtyResized"
        ])
    );

    let session = &runtime.session_by_id[&session_id];
    let new_tab_id = find_other_tab_id(&runtime, session_id, first_tab_id);
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        new_tab_id
    );
}

#[test]
fn new_tab_external_source_with_two_clients_is_ambiguous() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    attach_client(&mut session, ClientId::new(), first_tab_id, None);
    attach_client(&mut session, ClientId::new(), first_tab_id, None);
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
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
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
    let split_pane_id = find_other_pane_id(&runtime, session_id, &[existing_pane_id]);
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
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
    let new_tab = &runtime.session_by_id[&session_id].tabs
        [&find_other_tab_id(&runtime, session_id, existing_tab_id)];
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
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(&mut session, client_id, second_tab_id, Some(second_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Give the doomed tab a live PTY by splitting it while viewed.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let split_pane_id = find_other_pane_id(&runtime, session_id, &[first_pane_id, second_pane_id]);
    let remaining_client_revision_before_close = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("remaining client")
        .get_placement_revision();

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(second_tab_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            // PaneClosing and PaneRemoved for each of the two panes, TabClosed,
            // then TabFocused: the viewer moves to `first_tab_id`. Its pane has
            // no PTY, so no PtyResized follows.
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let session = &runtime.session_by_id[&session_id];
    assert!(!session.tabs.contains_key(&second_tab_id));
    assert!(session
        .panes
        .get_pane_record_by_id(second_pane_id)
        .is_none());
    assert!(session.panes.get_pane_record_by_id(split_pane_id).is_none());
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        first_tab_id
    );
    assert!(!runtime.live_pane_ids.contains(&split_pane_id));
    assert!(!runtime.pty_size_by_pane_id.contains_key(&split_pane_id));
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, split_pane_id),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION
        }]
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("remaining client")
            .get_placement_revision(),
        remaining_client_revision_before_close + 1
    );
}

#[test]
fn close_tab_kills_every_pane_concurrently() {
    // The doomed tab holds three panes (the PTY-less root plus two spawned
    // splits); the barrier releases a kill only once all three have started.
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender),
    )));
    fake_pty_backend.hold_kills_at(Arc::new(Barrier::new(3)));
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let mut runtime = Server::from_runtime_parts(pty_backend, runtime_event_receiver);

    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(&mut session, client_id, second_tab_id, Some(second_pane_id));
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
    let split_pane_ids: Vec<PaneId> = runtime.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .filter(|pane_id| *pane_id != first_pane_id && *pane_id != second_pane_id)
        .collect();
    let [first_split_pane_id, second_split_pane_id] = split_pane_ids[..] else {
        panic!("expected exactly two split panes, got {split_pane_ids:?}");
    };

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(second_tab_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "PaneClosing",
            "PaneRemoved",
            "PaneClosing",
            "PaneRemoved",
            "TabClosed",
            "TabFocused",
            "PaneFocused"
        ])
    );

    // Both live children receive their graceful kill. Each kill passes the
    // barrier only after both kills reach it.
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, first_split_pane_id),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION
        }]
    );
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, second_split_pane_id),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION
        }]
    );
}

#[test]
fn shutdown_returns_only_after_a_pane_kill_already_started_ends() {
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender),
    )));
    let kill_barrier = Arc::new(Barrier::new(2));
    fake_pty_backend.hold_kills_at(kill_barrier.clone());
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let mut runtime = Server::from_runtime_parts(pty_backend, runtime_event_receiver);
    let pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(
            pane_id,
            SpawnSpec::build_default_shell(None, BTreeMap::new()),
            PtySize {
                column_count: 80,
                row_count: 24,
            },
        )
        .expect("spawn");
    runtime.kill_pane_off_thread(pane_id, KillPolicy::Force);

    let (shutdown_done_sender, shutdown_done_receiver) = mpsc::channel();
    let shutdown_thread = thread::spawn(move || {
        runtime.shutdown();
        shutdown_done_sender
            .send(())
            .expect("the test waits for the shutdown");
    });

    // The kill waits at the barrier, so the shutdown cannot end yet.
    assert_eq!(
        shutdown_done_receiver.recv_timeout(Duration::from_millis(200)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    kill_barrier.wait();
    shutdown_done_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("the shutdown ends once the kill ends");
    shutdown_thread.join().expect("the shutdown thread ends");
    assert_eq!(
        fake_pty_backend.list_pane_kill_policies(pane_id),
        Ok(vec![KillPolicy::Force])
    );
}

#[test]
fn close_tab_with_a_busy_confirm_pane_rejects_without_force() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(&mut session, client_id, second_tab_id, Some(second_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let split_pane_id = find_other_pane_id(&runtime, session_id, &[first_pane_id, second_pane_id]);
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .panes
        .get_pane_record_mut_by_id(split_pane_id)
        .unwrap()
        .close_policy = PaneClosePolicy::ConfirmIfBusy;

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(second_tab_id),
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
    assert!(session.tabs.contains_key(&second_tab_id));
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(split_pane_id)
            .map(PaneRecord::get_lifecycle),
        Some(&PaneLifecycle::Running)
    );
    assert!(runtime.live_pane_ids.contains(&split_pane_id));
    assert!(fake_pty_backend
        .list_pane_kill_policies(split_pane_id)
        .unwrap()
        .is_empty());
}

#[test]
fn close_tab_force_kills_a_busy_confirm_pane() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(&mut session, client_id, second_tab_id, Some(second_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let split_pane_id = find_other_pane_id(&runtime, session_id, &[first_pane_id, second_pane_id]);
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .panes
        .get_pane_record_mut_by_id(split_pane_id)
        .unwrap()
        .close_policy = PaneClosePolicy::ConfirmIfBusy;

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(second_tab_id),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
    ));
    assert_eq!(
        get_command_outcome(&command_result),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "PaneClosing",
            "PaneRemoved",
            "TabClosed",
            "TabFocused",
            "PaneFocused"
        ])
    );
    assert!(!runtime.session_by_id[&session_id]
        .tabs
        .contains_key(&second_tab_id));
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, split_pane_id),
        vec![KillPolicy::Force]
    );
}

#[test]
fn close_tab_confirm_if_busy_exited_pane_closes() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(&mut session, client_id, second_tab_id, Some(second_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let split_pane_id = find_other_pane_id(&runtime, session_id, &[first_pane_id, second_pane_id]);
    {
        let pane_record = runtime
            .session_by_id
            .get_mut(&session_id)
            .unwrap()
            .panes
            .get_pane_record_mut_by_id(split_pane_id)
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
            tab_id: Some(second_tab_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    ));
    assert_eq!(
        get_command_outcome(&command_result),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "PaneClosing",
            "PaneRemoved",
            "TabClosed",
            "TabFocused",
            "PaneFocused"
        ])
    );
    assert!(!runtime.session_by_id[&session_id]
        .tabs
        .contains_key(&second_tab_id));
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, split_pane_id),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION
        }]
    );
}

#[test]
fn close_last_tab_quits_the_session() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs::default()),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            // PaneClosing, PaneRemoved, TabClosed, Quit.
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "TabClosed", "Quit"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert!(session.tabs.is_empty());
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn close_tab_with_an_unknown_explicit_tab_is_not_found() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
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
    let split_pane_id = find_other_pane_id(
        &runtime,
        session_id,
        &[remaining_root_pane_id, closing_root_pane_id],
    );
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

    // The moving client closes its tab and joins the remaining tab: 40x10 leaves a 40x8 pane
    // region, so each half's content is 18x6.
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
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
            .get_active_tab_id(),
        remaining_tab_id
    );
}

#[test]
fn move_tab_reorders_and_emits() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let third_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let third_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_pane_record(&mut session, third_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    register_session_tab(&mut session, third_tab_id, third_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The explicit `third_tab_id`, at index 2, moves to the front.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::MoveTab(MoveTabArgs {
            tab_id: Some(third_tab_id),
            target_tab_index: 0,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(list_event_names(&emitted_events), ["TabMoved"]);
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    // New order C, A, B — the others closed ranks behind the moved tab.
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs[&third_tab_id].get_tab_index(), 0);
    assert_eq!(session.tabs[&first_tab_id].get_tab_index(), 1);
    assert_eq!(session.tabs[&second_tab_id].get_tab_index(), 2);
    // Order-only change: the client still views the same tab.
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        first_tab_id
    );
}

#[test]
fn move_tab_defaults_to_the_issuers_active_tab() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    // The issuer views `second_tab_id`, at index 1.
    attach_client(&mut session, client_id, second_tab_id, Some(second_pane_id));
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs[&second_tab_id].get_tab_index(), 0);
    assert_eq!(session.tabs[&first_tab_id].get_tab_index(), 1);
}

#[test]
fn move_tab_clamps_an_out_of_range_index() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let third_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let third_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_pane_record(&mut session, third_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    register_session_tab(&mut session, third_tab_id, third_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Index 99 clamps to the last slot (2).
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::MoveTab(MoveTabArgs {
            tab_id: Some(first_tab_id),
            target_tab_index: 99,
        }),
    ));
    match command_result {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(list_event_names(&emitted_events), ["TabMoved"])
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs[&second_tab_id].get_tab_index(), 0);
    assert_eq!(session.tabs[&third_tab_id].get_tab_index(), 1);
    assert_eq!(session.tabs[&first_tab_id].get_tab_index(), 2);
}

#[test]
fn move_tab_to_its_current_slot_is_ok_with_no_events() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::MoveTab(MoveTabArgs {
            tab_id: Some(first_tab_id),
            target_tab_index: 0,
        }),
    ));
    match command_result {
        CommandResult::Ok { emitted_events, .. } => assert!(emitted_events.is_empty()),
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs[&first_tab_id].get_tab_index(), 0);
    assert_eq!(session.tabs[&second_tab_id].get_tab_index(), 1);
}

#[test]
fn in_session_cli_move_tab_defaults_to_the_source_pane_tab() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(session_id);
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    // The client's active tab is `second_tab_id`. The CLI command comes from
    // `first_pane_id`, in `first_tab_id`.
    attach_client(&mut session, client_id, second_tab_id, None);
    runtime.session_by_id.insert(session.session_id, session);

    // MoveTab with no explicit tab: an in-session CLI source resolves to the
    // tab that holds `first_pane_id`, not to the client's active tab.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(client_id),
        first_pane_id,
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.tabs[&second_tab_id].get_tab_index(), 0);
    assert_eq!(session.tabs[&first_tab_id].get_tab_index(), 1);
}

#[test]
fn move_tab_with_an_unknown_tab_is_rejected() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
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
        runtime.session_by_id[&session_id].tabs[&first_tab_id].get_tab_index(),
        0
    );
}

// --- FocusTab handler ----------------------------------------------------------

#[test]
fn focus_tab_switches_the_view_and_emits() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(second_tab_id),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            // The client held no focus in the tab it switches to, so it lands
            // on that tab's landing pane. Neither tab holds a live PTY to reflow.
            assert_eq!(
                list_event_names(&emitted_events),
                ["TabFocused", "PaneFocused"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        second_tab_id
    );
}

#[test]
fn focus_tab_index_next_and_previous_resolve_against_the_display_order() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id); // index 0
    register_session_tab(&mut session, second_tab_id, second_pane_id); // index 1
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    let get_active_tab_id = |runtime: &Server| {
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id()
    };

    // Next from `first_tab_id` (index 0) steps to `second_tab_id` (index 1).
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Next,
            client_id: None,
        }),
    ));
    assert_eq!(
        get_command_outcome(&command_result),
        Ok(vec!["TabFocused", "PaneFocused"])
    );
    assert_eq!(get_active_tab_id(&runtime), second_tab_id);

    // Next from the last tab wraps to the first.
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Next,
            client_id: None,
        }),
    ));
    assert_eq!(get_command_outcome(&command_result), Ok(vec!["TabFocused"]));
    assert_eq!(get_active_tab_id(&runtime), first_tab_id);

    // Previous from the first tab wraps to the last.
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Previous,
            client_id: None,
        }),
    ));
    assert_eq!(get_command_outcome(&command_result), Ok(vec!["TabFocused"]));
    assert_eq!(get_active_tab_id(&runtime), second_tab_id);

    // An explicit index resolves the tab at that display position.
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Index(0),
            client_id: None,
        }),
    ));
    assert_eq!(get_command_outcome(&command_result), Ok(vec!["TabFocused"]));
    assert_eq!(get_active_tab_id(&runtime), first_tab_id);
}

#[test]
fn focus_tab_on_the_already_active_tab_is_ok_with_no_events() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(first_tab_id),
            client_id: None,
        }),
    ));
    match command_result {
        CommandResult::Ok { emitted_events, .. } => assert!(emitted_events.is_empty()),
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        first_tab_id
    );
}

#[test]
fn focus_tab_with_an_unknown_id_or_index_is_not_found() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
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
            .get_active_tab_id(),
        first_tab_id
    );
}

#[test]
fn focus_tab_explicit_client_wins_over_the_issuer() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let issuer_client_id = ClientId::new();
    let named_client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(
        &mut session,
        issuer_client_id,
        first_tab_id,
        Some(first_pane_id),
    );
    attach_client(
        &mut session,
        named_client_id,
        first_tab_id,
        Some(first_pane_id),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(issuer_client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(second_tab_id),
            client_id: Some(named_client_id),
        }),
    ));
    assert_eq!(
        get_command_outcome(&command_result),
        Ok(vec!["TabFocused", "PaneFocused"])
    );

    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(named_client_id)
            .unwrap()
            .get_active_tab_id(),
        second_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(issuer_client_id)
            .unwrap()
            .get_active_tab_id(),
        first_tab_id
    );
}

#[test]
fn focus_tab_with_an_unattached_explicit_client_is_rejected() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(second_tab_id),
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
            .get_active_tab_id(),
        first_tab_id
    );
}

#[test]
fn focus_tab_external_source_defaults_to_the_sole_client() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(second_tab_id),
            client_id: None,
        }),
    ));
    assert_eq!(
        get_command_outcome(&command_result),
        Ok(vec!["TabFocused", "PaneFocused"])
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        second_tab_id
    );
}

#[test]
fn focus_tab_external_source_with_two_clients_is_ambiguous() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    attach_client(&mut session, ClientId::new(), first_tab_id, None);
    attach_client(&mut session, ClientId::new(), first_tab_id, None);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(first_tab_id),
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
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(first_tab_id),
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
    let split_pane_id = find_other_pane_id(
        &runtime,
        session_id,
        &[moving_client_root_pane_id, staying_client_root_pane_id],
    );
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

    // The moving client switches onto the staying client's tab: full viewport minimum is 30x8,
    // leaving a 30x6 pane region; each half's content is 13x4.
    let command_result = runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(moving_client_id),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(staying_client_tab_id),
            client_id: None,
        }),
    ));
    match command_result {
        CommandResult::Ok { emitted_events, .. } => {
            // TabFocused, PaneFocused, then the resize of the tightened PTY.
            // The pane of `moving_client_tab_id` has no PTY.
            assert_eq!(
                list_event_names(&emitted_events),
                ["TabFocused", "PaneFocused", "PtyResized"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
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
    assert_eq!(
        get_command_outcome(&command_result),
        Ok(vec!["TabFocused", "PaneFocused", "PtyResized"])
    );
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
    let first_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
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
        first_tab_id,
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
            reason: RejectReason::MinimumSize,
            help: Some("not enough space for a new tab".to_string()),
        }
    );
    assert_eq!(runtime.session_by_id[&session_id].tabs.len(), 1);
    assert!(fake_pty_backend.list_spawned_pane_ids().is_empty());
}

/// The [`build_resize_fixture`] tab with its client zoomed onto the split pane
/// through dispatch: mode `Fullscreen { focused_pane_id: split_pane_id }`, the
/// split pane's PTY resized to the full-tab content rect (80x24 viewport ->
/// 78x20). `split_pane_pty_size` stays the spawn-time size.
fn build_fullscreen_fixture() -> ResizeFixture {
    let mut fullscreen_fixture = build_resize_fixture();
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(fullscreen_fixture.client_id),
        Command::TogglePaneFullscreen,
    );
    assert_eq!(
        get_command_outcome(&fullscreen_fixture.runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged", "PtyResized"])
    );
    fullscreen_fixture
}

/// The id of the single tab of `session_id`. Panics unless the session holds
/// exactly one tab.
fn get_only_tab_id(runtime: &Server, session_id: SessionId) -> TabId {
    let tabs = &runtime.session_by_id[&session_id].tabs;
    assert_eq!(tabs.len(), 1, "exactly one tab");
    *tabs.keys().next().expect("exactly one tab")
}

/// The id of the single tab of `session_id` other than `known_tab_id`: after
/// one `new-tab` in a one-tab session, the new tab. Panics unless exactly one
/// such tab exists.
fn find_other_tab_id(runtime: &Server, session_id: SessionId, known_tab_id: TabId) -> TabId {
    let mut other_tab_ids = runtime.session_by_id[&session_id]
        .tabs
        .keys()
        .copied()
        .filter(|tab_id| *tab_id != known_tab_id);
    let other_tab_id = other_tab_ids.next().expect("another tab exists");
    assert_eq!(other_tab_ids.next(), None, "exactly one other tab");
    other_tab_id
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
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            // LayoutChanged plus the promoted pane's PtyResized; the hidden
            // root has no PTY and the focus was already on the pane.
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PtyResized"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: split_pane_id
        }
    );
    // The mode is a solve-time overlay: the tree itself is untouched.
    assert_eq!(*session.tabs[&tab_id].get_layout_tree(), tree_before);
    let fullscreen_pty_size = PtySize {
        column_count: 78,
        row_count: 20,
    };
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        fullscreen_pty_size
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        fullscreen_pty_size
    );
}

#[test]
fn toggle_fullscreen_off_restores_the_exact_prior_layout_and_sizes() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        client_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_fullscreen_fixture();
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Tiled
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(*session.tabs[&tab_id].get_layout_tree(), tree_before);
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        split_pane_pty_size
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        split_pane_pty_size
    );
}

#[test]
fn toggle_fullscreen_from_the_issuing_pane_moves_the_acting_focus() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        root_pane_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
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
            .get_focused_pane_id(tab_id),
        Some(root_pane_id)
    );
    assert_eq!(
        session.tabs[&tab_id].list_focus_mru().first(),
        Some(&root_pane_id)
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        split_pane_pty_size
    );
}

#[test]
fn toggle_fullscreen_on_an_unviewed_tab_is_rejected() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // Issued from inside the pane of the tab that no client views: the toggle
    // is refused, and the tab stays tiled.
    let command_source = CommandSource::from_in_session_cli(
        session_id,
        Some(client_id),
        second_pane_id,
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
        get_client_layout_mode(&runtime, session_id, client_id, second_tab_id),
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
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        root_pane_id,
        split_pane_id,
        ..
    } = build_fullscreen_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);
    let fullscreen_pty_size = PtySize {
        column_count: 78,
        row_count: 20,
    };

    // The fullscreen hides the root pane. Focusing the root pane moves the zoom
    // onto it, and the mode stays fullscreen.
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
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
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
            .get_focused_pane_id(tab_id),
        Some(root_pane_id)
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        fullscreen_pty_size
    );
}

#[test]
fn focus_pane_retargeting_back_skips_the_unchanged_pty() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        client_id,
        root_pane_id,
        split_pane_id,
        ..
    } = build_fullscreen_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);
    let fullscreen_pty_size = PtySize {
        column_count: 78,
        row_count: 20,
    };

    let build_focus_command_envelope = |pane_id: PaneId| {
        build_command_envelope(
            CommandSource::from_key_binding(client_id),
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Pane(pane_id),
                client_id: None,
            }),
        )
    };
    assert_eq!(
        get_command_outcome(&runtime.dispatch(build_focus_command_envelope(root_pane_id))),
        Ok(vec!["LayoutChanged", "PaneFocused"])
    );
    let resize_count_before_retarget = fake_pty_backend
        .list_pane_sizes(split_pane_id)
        .unwrap()
        .len();

    // Retargeting back gives the pane the same full-tab rect it last held,
    // so the reflow applies nothing.
    match runtime.dispatch(build_focus_command_envelope(split_pane_id)) {
        CommandResult::Ok { emitted_events, .. } => {
            // LayoutChanged plus PaneFocused, no PtyResized.
            assert_eq!(
                list_event_names(&emitted_events),
                ["LayoutChanged", "PaneFocused"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: split_pane_id
        }
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        fullscreen_pty_size
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(split_pane_id)
            .unwrap()
            .len(),
        resize_count_before_retarget
    );
}

#[test]
fn focus_pane_on_the_promoted_pane_is_a_no_op() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        split_pane_id,
        ..
    } = build_fullscreen_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(split_pane_id),
            client_id: None,
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(emitted_events, Vec::new());
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: split_pane_id
        }
    );
}

/// One client zooming a pane changes nothing for another client on the same tab.
/// The first client zooms its pane. The second client keeps its tiled view, its
/// own focus, and its own pane on screen.
#[test]
fn one_clients_zoom_leaves_another_clients_view_alone() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let first_client_id = ClientId::new();
    let second_client_id = ClientId::new();
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
    attach_client(&mut session, first_client_id, tab_id, Some(focused_pane_id));
    attach_client(&mut session, second_client_id, tab_id, Some(other_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The first client zooms the pane it has focused.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(first_client_id),
        Command::TogglePaneFullscreen,
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged"])
    );

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, first_client_id, tab_id),
        LayoutMode::Fullscreen { focused_pane_id },
        "the client that zoomed sees its pane filling the tab"
    );
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, second_client_id, tab_id),
        LayoutMode::Tiled,
        "the other client keeps its tiled view: another client's zoom is not its business"
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(second_client_id)
            .unwrap()
            .get_focused_pane_id(tab_id),
        Some(other_pane_id),
        "and keeps its own focus"
    );

    // B re-focuses the pane it already holds: nothing about its view changed, so
    // nothing at all happens.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(second_client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(other_pane_id),
            client_id: None,
        }),
    );
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok { emitted_events, .. } => {
            assert_eq!(emitted_events, Vec::new());
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, second_client_id, tab_id),
        LayoutMode::Tiled
    );
}

/// A pane's PTY has one size, and each client that draws the pane has its own
/// rect for it: one client zooms it, another shows it tiled. The child gets the
/// smallest of those rects.
///
/// The split pane is 38x20 tiled. The first client zooms it, and the second
/// client still views the tab tiled. The child stays 38x20: the first client
/// sees the pane alone on screen, at the size the second client shows.
#[test]
fn a_zoom_does_not_grow_a_pane_another_client_still_shows_tiled() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id: first_client_id,
        root_pane_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);

    // A second client views the same tab, tiled, focused on the other pane.
    let second_client_id = ClientId::new();
    attach_client(
        runtime.session_by_id.get_mut(&session_id).expect("session"),
        second_client_id,
        tab_id,
        Some(root_pane_id),
    );

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(first_client_id),
        Command::TogglePaneFullscreen,
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged"])
    );

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, first_client_id, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: split_pane_id
        }
    );
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, second_client_id, tab_id),
        LayoutMode::Tiled
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id], split_pane_pty_size,
        "the second client still draws the split pane tiled at 38x20"
    );
}

/// The tiled client leaving takes its claim on the pane's size with it: the pane
/// is then drawn only by the client that has it zoomed, and the child grows to
/// fill the tab.
#[test]
fn a_zoomed_pane_grows_once_the_client_holding_it_tiled_detaches() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id: first_client_id,
        root_pane_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);

    let second_client_id = ClientId::new();
    attach_client(
        runtime.session_by_id.get_mut(&session_id).expect("session"),
        second_client_id,
        tab_id,
        Some(root_pane_id),
    );

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(first_client_id),
        Command::TogglePaneFullscreen,
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged"])
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id], split_pane_pty_size,
        "the second client still shows the split pane tiled at 38x20"
    );

    // The second client detaches. The first client, still zoomed, is the only
    // client that draws `split_pane_id`.
    runtime.handle_client_detach(second_client_id);

    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged"])
    );

    // Focus the pane the zoom is hiding: the zoom follows the focus onto it.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(other_pane_id),
            client_id: None,
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged", "PaneFocused"])
    );

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
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        root_pane_id,
        split_pane_id,
        ..
    } = build_fullscreen_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);

    // Splitting the promoted pane: the tab returns to the tiled view and
    // both halves of the split are sized against it (root 40, the split
    // pair 20 each -> 18x20 content after chrome rows).
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized",
            "PtyResized"
        ])
    );

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Tiled
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id, split_pane_id]);
    let split_pair_pty_size = PtySize {
        column_count: 18,
        row_count: 20,
    };
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        split_pair_pty_size
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&new_pane_id],
        split_pair_pty_size
    );
}

#[test]
fn resize_pane_drops_the_resizing_clients_zoom() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_fullscreen_fixture();
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged", "PtyResized"])
    );

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Tiled
    );
    let expected_pty_size = PtySize {
        column_count: split_pane_pty_size.column_count + 5,
        row_count: split_pane_pty_size.row_count,
    };
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        expected_pty_size
    );
}

#[test]
fn resize_pane_from_outside_drops_the_zoom_of_the_client_it_names() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_fullscreen_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);
    let resize_pane_command = Command::ResizePane(ResizePaneArgs {
        pane_id: Some(split_pane_id),
        direction: Direction::Left,
        resize_amount_cells: 5,
    });

    // Naming no client: the client keeps its zoom, and the zoomed pane keeps
    // the whole tab.
    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        resize_pane_command.clone(),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged"])
    );
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: split_pane_id,
        }
    );

    // Naming the client: its zoom drops, and the pane takes its tiled size,
    // grown by both resizes.
    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), Some(client_id)),
        resize_pane_command,
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged", "PtyResized"])
    );
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Tiled
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        PtySize {
            column_count: split_pane_pty_size.column_count + 10,
            row_count: split_pane_pty_size.row_count,
        }
    );
}

#[test]
fn close_pane_drops_the_closing_clients_zoom() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        root_pane_id,
        split_pane_id,
        ..
    } = build_fullscreen_fixture();
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["PaneClosing", "PaneRemoved", "LayoutChanged"])
    );

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Tiled
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        *session.tabs[&tab_id].get_layout_tree(),
        LayoutNode::Pane(split_pane_id)
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        fullscreen_pty_size
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(tab_id),
        Some(split_pane_id)
    );
}

#[test]
fn close_pane_closing_the_promoted_pane_drops_the_fullscreen() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        root_pane_id,
        split_pane_id,
        ..
    } = build_fullscreen_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(split_pane_id),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "LayoutChanged",
            "PaneFocused"
        ])
    );

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
            .get_focused_pane_id(tab_id),
        Some(root_pane_id)
    );
}

/// The grid dimensions of the terminal engine of `pane_id`.
fn get_terminal_engine_dimensions(runtime: &Server, pane_id: PaneId) -> (u16, u16) {
    runtime.list_terminal_engines()[&pane_id]
        .get_terminal_state()
        .get_active_grid()
        .get_grid_dimensions()
}

#[test]
fn new_pane_installs_a_terminal_engine_at_spawn_size() {
    let ResizeFixture {
        runtime,
        root_pane_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();

    assert!(runtime.list_terminal_engines().contains_key(&split_pane_id));
    assert_eq!(
        get_terminal_engine_dimensions(&runtime, split_pane_id),
        (
            split_pane_pty_size.row_count,
            split_pane_pty_size.column_count
        ),
        "engine grid matches the spawned PTY size"
    );
    // The fixture's root pane never spawned a PTY, so it has no engine.
    assert!(!runtime.list_terminal_engines().contains_key(&root_pane_id));
}

#[test]
fn close_pane_removes_the_terminal_engine() {
    let ResizeFixture {
        mut runtime,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    assert!(runtime.list_terminal_engines().contains_key(&split_pane_id));

    // No explicit pane: the focused (split) pane closes and its engine goes
    // with its PTY bookkeeping.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ClosePane(ClosePaneArgs::default()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "LayoutChanged",
            "PaneFocused"
        ])
    );

    assert!(!runtime.list_terminal_engines().contains_key(&split_pane_id));
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "TabCreated",
            "PaneCreated",
            "TabFocused",
            "PaneFocused",
            "PtyResized"
        ])
    );

    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "TabCreated",
            "PaneCreated",
            "TabFocused",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
    assert!(runtime.list_terminal_engines().contains_key(&new_pane_id));

    // Closing the client's active tab (the new one) drops its pane's engine.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::CloseTab(CloseTabArgs::default()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "TabClosed",
            "TabFocused"
        ])
    );

    assert!(!runtime.list_terminal_engines().contains_key(&new_pane_id));
}

#[test]
fn resize_pane_resizes_the_terminal_engine_with_its_pty() {
    let ResizeFixture {
        mut runtime,
        client_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Left,
            resize_amount_cells: 5,
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged", "PtyResized"])
    );

    // The reflow grows the PTY of `split_pane_id` by 5 columns, and its engine
    // grid takes the same size.
    let expected_pty_size = PtySize {
        column_count: split_pane_pty_size.column_count + 5,
        row_count: split_pane_pty_size.row_count,
    };
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        expected_pty_size
    );
    assert_eq!(
        get_terminal_engine_dimensions(&runtime, split_pane_id),
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);

    // The split pane's child exits with code 0.
    let emitted_events = runtime.handle_child_exit(new_pane_id, ExitStatus::ExitCode(0));

    // The exit is reported first, then the removal, and the client's focus
    // moves to the root pane.
    assert_eq!(
        list_event_names(&emitted_events),
        [
            "PaneProcessExited",
            "PaneClosing",
            "PaneRemoved",
            "LayoutChanged",
            "PaneFocused"
        ]
    );
    assert_eq!(
        emitted_events[0],
        Event::PaneProcessExited(PaneProcessExited {
            pane_id: new_pane_id,
            exit_code: Some(0),
            signal: None,
        })
    );

    // The pane is gone from state and from every runtime map.
    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(new_pane_id)
        .is_none());
    assert!(!runtime.live_pane_ids.contains(&new_pane_id));
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
fn child_exit_advances_the_session_revision_when_one_client_revision_is_saturated() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let first_client_id = ClientId::new();
    let second_client_id = ClientId::new();
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, root_pane_id);
    register_session_tab(&mut session, tab_id, root_pane_id);
    attach_client(&mut session, first_client_id, tab_id, Some(root_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    assert_eq!(
        get_command_outcome(&runtime.dispatch(build_command_envelope(
            CommandSource::from_key_binding(first_client_id),
            Command::NewPane(build_new_pane_args()),
        ))),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let exiting_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
    attach_client(
        runtime
            .session_by_id
            .get_mut(&session_id)
            .expect("session exists"),
        second_client_id,
        tab_id,
        Some(exiting_pane_id),
    );

    replace_persisted_placement_revisions(&mut runtime, session_id, second_client_id, 0, u64::MAX);
    let first_client_revision_before = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(first_client_id)
        .expect("the first client exists")
        .get_placement_revision();

    let emitted_events = runtime.handle_child_exit(exiting_pane_id, ExitStatus::ExitCode(0));

    assert_eq!(
        list_event_names(&emitted_events),
        [
            "PaneProcessExited",
            "PaneClosing",
            "PaneRemoved",
            "LayoutChanged",
            "PaneFocused",
            "PaneFocused"
        ]
    );
    assert_eq!(
        emitted_events[0],
        Event::PaneProcessExited(PaneProcessExited {
            pane_id: exiting_pane_id,
            exit_code: Some(0),
            signal: None,
        })
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.get_placement_revision(), 1);
    assert_eq!(
        session
            .clients
            .get_client_by_id(first_client_id)
            .expect("the first client survives")
            .get_placement_revision(),
        first_client_revision_before + 1
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(second_client_id)
            .expect("the second client survives")
            .get_placement_revision(),
        u64::MAX
    );
}

#[test]
fn client_attach_registers_a_client_when_the_session_revision_is_saturated() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let bootstrap_client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, pane_id) = get_only_session_slot(&runtime);
    replace_persisted_placement_revisions(
        &mut runtime,
        session_id,
        bootstrap_client_id,
        u64::MAX,
        0,
    );

    let joining_client_id = ClientId::new();
    let emitted_events = runtime.handle_client_attach(
        session_id,
        joining_client_id,
        viewport_size,
        None,
        tab_id,
        None,
        SystemTime::now(),
        false,
    );

    assert_eq!(
        emitted_events,
        vec![Event::PaneFocused(PaneFocused {
            client_id: joining_client_id,
            tab_id: Some(tab_id),
            pane_id,
            previous_pane_id: None,
        })]
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(joining_client_id)
            .map(Client::get_active_tab_id),
        Some(tab_id)
    );
    assert_eq!(session.get_placement_revision(), u64::MAX);
    assert_eq!(
        session
            .clients
            .get_client_by_id(bootstrap_client_id)
            .expect("bootstrap client survives")
            .get_placement_revision(),
        1
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(joining_client_id)
            .expect("joining client is registered")
            .get_placement_revision(),
        0
    );
}

#[test]
fn client_detach_removes_a_client_when_the_session_revision_is_saturated() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, _pane_id) = get_only_session_slot(&runtime);
    replace_persisted_placement_revisions(&mut runtime, session_id, client_id, u64::MAX, 0);

    runtime.handle_client_detach(client_id);

    let session = &runtime.session_by_id[&session_id];
    assert!(session.clients.get_client_by_id(client_id).is_none());
    assert_eq!(session.get_placement_revision(), u64::MAX);
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

    let emitted_events = runtime.handle_child_exit(root_pane_id, ExitStatus::ExitCode(0));

    // Removing the last pane closes the tab, and closing the last tab quits.
    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(root_pane_id)
        .is_none());
    assert!(runtime.session_by_id[&session_id].tabs.is_empty());
    assert_eq!(
        list_event_names(&emitted_events),
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
    let landing_client_id = ClientId::new();
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(
        &mut session,
        landing_client_id,
        second_tab_id,
        Some(second_pane_id),
    );
    attach_client(&mut session, client_id, first_tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // The sole pane of `first_tab_id` exits: `first_tab_id` closes, and
    // `second_tab_id` survives. The session keeps running, and the viewer moves
    // to `second_tab_id`, which reflows.
    let emitted_events = runtime.handle_child_exit(first_pane_id, ExitStatus::ExitCode(0));

    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(first_pane_id)
        .is_none());
    assert!(!runtime.session_by_id[&session_id]
        .tabs
        .contains_key(&first_tab_id));
    assert!(runtime.session_by_id[&session_id]
        .tabs
        .contains_key(&second_tab_id));
    assert_eq!(
        list_event_names(&emitted_events),
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
            .get_active_tab_id(),
        second_tab_id
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(landing_client_id)
            .expect("landing client")
            .get_placement_revision(),
        1
    );
}

#[test]
fn child_exit_of_an_unknown_pane_is_dropped() {
    let (mut runtime, _runtime_event_sender) = build_runtime();

    // No session owns the pane (closed while its exit waited in the inbox).
    let emitted_events = runtime.handle_child_exit(PaneId::new(), ExitStatus::ExitCode(0));

    assert!(emitted_events.is_empty());
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let new_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);

    let coded_exit_events = runtime.handle_child_exit(root_pane_id, ExitStatus::ExitCode(3));
    assert_eq!(
        coded_exit_events.first(),
        Some(&Event::PaneProcessExited(PaneProcessExited {
            pane_id: root_pane_id,
            exit_code: Some(3),
            signal: None,
        }))
    );
    assert_eq!(
        list_event_names(&coded_exit_events),
        [
            "PaneProcessExited",
            "PaneClosing",
            "PaneRemoved",
            "LayoutChanged",
            "PtyResized"
        ]
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
    assert_eq!(runtime.session_by_id.len(), 1, "exactly one session");
    let session = runtime
        .session_by_id
        .values()
        .next()
        .expect("exactly one session");
    assert_eq!(session.tabs.len(), 1, "exactly one tab");
    assert_eq!(session.panes.count_pane_records(), 1, "exactly one pane");
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
    let bootstrap_client_id = runtime
        .bootstrap_local(SessionId::new(), large_viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, pane_id) = get_only_session_slot(&runtime);
    let resize_count_before_attach = fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("pane spawned")
        .len();

    // A smaller second client attaches to the same tab: the tab size drops
    // to the per-axis minimum, so the live pane's PTY reflows down.
    let joining_client_id = ClientId::new();
    let emitted_events = runtime.handle_client_attach(
        session_id,
        joining_client_id,
        small_viewport_size,
        None,
        tab_id,
        None,
        SystemTime::now(),
        false,
    );

    let expected_pty_size = compute_root_pane_pty_size(
        pane_id,
        compute_default_pane_area_size(small_viewport_size),
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
        emitted_events,
        vec![
            Event::PaneFocused(PaneFocused {
                client_id: joining_client_id,
                tab_id: Some(tab_id),
                pane_id,
                previous_pane_id: None,
            }),
            Event::PtyResized(PtyResized {
                pane_id,
                pty_size: expected_pty_size,
            })
        ]
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.get_placement_revision(), 1);
    assert_eq!(
        session
            .clients
            .get_client_by_id(bootstrap_client_id)
            .expect("genesis client")
            .get_placement_revision(),
        1
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(joining_client_id)
            .expect("joining client")
            .get_placement_revision(),
        0
    );
}

/// One session seeded for alice, holding a tiled pane and one live floating
/// pane.
struct FloatingPaneFixture {
    runtime: Server,
    fake_pty_backend: Arc<FakePtyBackend>,
    session_id: SessionId,
    alice_client_id: ClientId,
    tab_id: TabId,
    tiled_pane_id: PaneId,
    floating_pane_id: PaneId,
}

/// A floating pane size of 60% of the floating viewport on both axes.
fn build_sixty_percent_floating_pane_size() -> FloatingPaneSize {
    let sixty_percent =
        FloatingPaneDimension::Percent(AxisPercent::try_from(60).expect("60 is a percent"));
    FloatingPaneSize {
        width: sixty_percent,
        height: sixty_percent,
    }
}

/// A session seeded by [`Server::bootstrap_local`] for alice, attached at
/// `UNIX_EPOCH` with a `120x42` viewport: pane area `120x40`, tiled pane PTY
/// `118x38`. It also holds one live floating pane asking for 60% by 60%,
/// solved for alice alone: outer `72x24`, PTY `70x20`.
fn build_floating_pane_fixture() -> FloatingPaneFixture {
    let FloatingCommandFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        alice_client_id,
        tab_id,
        tiled_pane_id,
    } = bootstrap_alice_session(Size {
        column_count: 120,
        row_count: 42,
    });
    let floating_pane_id = PaneId::new();
    let session = runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("the seeded session");
    register_pane_record(session, floating_pane_id);
    session
        .floating_set
        .add_member(FloatingMember {
            pane_id: floating_pane_id,
            desired_size: build_sixty_percent_floating_pane_size(),
            solved_size: FloatingPaneSizeSolve::Sized(Size {
                column_count: 72,
                row_count: 24,
            }),
        })
        .expect("the floating set is empty");
    let floating_pty_size = PtySize {
        column_count: 70,
        row_count: 20,
    };
    fake_pty_backend
        .spawn_pane(floating_pane_id, build_spawn_spec(), floating_pty_size)
        .expect("spawn the floating pane");
    runtime.park_pane_pty(floating_pane_id, floating_pty_size);
    FloatingPaneFixture {
        runtime,
        fake_pty_backend,
        session_id,
        alice_client_id,
        tab_id,
        tiled_pane_id,
        floating_pane_id,
    }
}

/// `PtyResized` for `pane_id` at `column_count` by `row_count`.
fn build_pty_resized(pane_id: PaneId, column_count: u16, row_count: u16) -> Event {
    Event::PtyResized(PtyResized {
        pane_id,
        pty_size: PtySize {
            column_count,
            row_count,
        },
    })
}

#[test]
fn a_smaller_client_shrinks_every_floating_pane_and_its_detach_regrows_them() {
    let FloatingPaneFixture {
        mut runtime,
        session_id,
        tab_id,
        tiled_pane_id,
        floating_pane_id,
        ..
    } = build_floating_pane_fixture();
    assert_eq!(
        runtime.pty_size_by_pane_id[&tiled_pane_id],
        PtySize {
            column_count: 118,
            row_count: 38,
        }
    );
    let bob_client_id = ClientId::new();

    // bob's 80x24 viewport gives an 80x22 pane area: 60% of it is 48x13.
    let attach_events = runtime.handle_client_attach(
        session_id,
        bob_client_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        tab_id,
        None,
        SystemTime::now(),
        false,
    );

    assert_eq!(
        attach_events,
        vec![
            Event::PaneFocused(PaneFocused {
                client_id: bob_client_id,
                tab_id: Some(tab_id),
                pane_id: tiled_pane_id,
                previous_pane_id: None,
            }),
            build_pty_resized(tiled_pane_id, 78, 20),
            build_pty_resized(floating_pane_id, 46, 9),
        ]
    );
    assert_eq!(
        get_floating_member(&runtime, session_id, floating_pane_id).solved_size,
        FloatingPaneSizeSolve::Sized(Size {
            column_count: 48,
            row_count: 13,
        })
    );

    let detach_events = runtime.handle_client_detach(bob_client_id);

    assert_eq!(
        detach_events,
        vec![
            build_pty_resized(tiled_pane_id, 118, 38),
            build_pty_resized(floating_pane_id, 70, 20),
        ]
    );
    assert_eq!(
        get_floating_member(&runtime, session_id, floating_pane_id).solved_size,
        FloatingPaneSizeSolve::Sized(Size {
            column_count: 72,
            row_count: 24,
        })
    );
}

#[test]
fn dropping_a_client_that_did_not_attach_again_regrows_every_floating_pane() {
    let FloatingPaneFixture {
        mut runtime,
        session_id,
        tab_id,
        tiled_pane_id,
        floating_pane_id,
        ..
    } = build_floating_pane_fixture();
    let bob_client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        bob_client_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        tab_id,
        None,
        SystemTime::now(),
        false,
    );
    // bob's record came across an update, and bob does not attach again.
    runtime.client_ids_awaiting_reconnect.insert(bob_client_id);

    let emitted_events = runtime.handle_drop_unclaimed_clients(Instant::now());

    assert_eq!(
        emitted_events,
        vec![
            build_pty_resized(tiled_pane_id, 118, 38),
            build_pty_resized(floating_pane_id, 70, 20),
        ]
    );
    assert_eq!(
        get_floating_member(&runtime, session_id, floating_pane_id).solved_size,
        FloatingPaneSizeSolve::Sized(Size {
            column_count: 72,
            row_count: 24,
        })
    );
}

#[test]
fn a_client_too_small_for_the_floating_minimum_suppresses_the_pane_and_keeps_its_pty() {
    let FloatingPaneFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        tab_id,
        tiled_pane_id,
        floating_pane_id,
        ..
    } = build_floating_pane_fixture();
    // `pane { min-cols 20 min-rows 6 }`: the floating minimum is 22x10.
    runtime.config.pane.minimum_column_count = 20;
    runtime.config.pane.minimum_row_count = 6;
    let bob_client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        bob_client_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        tab_id,
        None,
        SystemTime::now(),
        false,
    );
    let floating_pane_size_count = fake_pty_backend
        .list_pane_sizes(floating_pane_id)
        .expect("the floating pane spawned")
        .len();

    // A 21-column pane area cannot hold the 22-column floating minimum, and
    // cannot hold the tiled pane's 22-column outer minimum either.
    let shrink_events = runtime.handle_client_resize(
        bob_client_id,
        Size {
            column_count: 21,
            row_count: 32,
        },
        Some(PaneArea::Reported(Size {
            column_count: 21,
            row_count: 30,
        })),
        None,
    );

    assert_eq!(shrink_events, Vec::new());
    assert_eq!(
        get_floating_member(&runtime, session_id, floating_pane_id).solved_size,
        FloatingPaneSizeSolve::Suppressed
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&floating_pane_id],
        PtySize {
            column_count: 46,
            row_count: 9,
        }
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(floating_pane_id)
            .expect("the floating pane spawned")
            .len(),
        floating_pane_size_count
    );
    // A suppressed floating pane's output is still read.
    runtime.handle_pty_output(floating_pane_id, b"top");
    let floating_grid = runtime.list_terminal_engines()[&floating_pane_id]
        .get_terminal_state()
        .get_active_grid();
    assert_eq!(
        [0, 1, 2].map(|column_index| floating_grid
            .get_cell(0, column_index)
            .expect("cell in bounds")
            .get_character()),
        ['t', 'o', 'p']
    );

    // 60% of a 100x30 pane area is 60x18.
    let regrow_events = runtime.handle_client_resize(
        bob_client_id,
        Size {
            column_count: 100,
            row_count: 32,
        },
        Some(PaneArea::Reported(Size {
            column_count: 100,
            row_count: 30,
        })),
        None,
    );

    assert_eq!(
        regrow_events,
        vec![
            build_pty_resized(tiled_pane_id, 98, 28),
            build_pty_resized(floating_pane_id, 58, 14),
        ]
    );
    let floating_member = runtime.session_by_id[&session_id]
        .floating_set
        .list_members()[0];
    assert_eq!(
        floating_member.solved_size,
        FloatingPaneSizeSolve::Sized(Size {
            column_count: 60,
            row_count: 18,
        })
    );
    assert_eq!(
        floating_member.desired_size,
        build_sixty_percent_floating_pane_size()
    );
}

#[test]
fn a_starving_client_beside_a_roomy_one_leaves_every_floating_pane_as_it_is() {
    let FloatingPaneFixture {
        mut runtime,
        session_id,
        tab_id,
        tiled_pane_id,
        floating_pane_id,
        ..
    } = build_floating_pane_fixture();
    let bob_client_id = ClientId::new();

    let attach_events = runtime.handle_client_attach(
        session_id,
        bob_client_id,
        Size {
            column_count: 10,
            row_count: 3,
        },
        Some(PaneArea::Starving),
        tab_id,
        None,
        SystemTime::now(),
        false,
    );

    assert_eq!(
        attach_events,
        vec![Event::PaneFocused(PaneFocused {
            client_id: bob_client_id,
            tab_id: Some(tab_id),
            pane_id: tiled_pane_id,
            previous_pane_id: None,
        })]
    );
    assert_eq!(
        get_floating_member(&runtime, session_id, floating_pane_id).solved_size,
        FloatingPaneSizeSolve::Sized(Size {
            column_count: 72,
            row_count: 24,
        })
    );
}

#[test]
fn every_client_starving_freezes_every_floating_pane_until_one_reports_room() {
    let FloatingPaneFixture {
        mut runtime,
        session_id,
        alice_client_id,
        tiled_pane_id,
        floating_pane_id,
        ..
    } = build_floating_pane_fixture();
    let alice_viewport_size = Size {
        column_count: 120,
        row_count: 42,
    };

    let starving_events = runtime.handle_client_resize(
        alice_client_id,
        alice_viewport_size,
        Some(PaneArea::Starving),
        None,
    );

    assert_eq!(starving_events, Vec::new());
    assert_eq!(
        get_floating_member(&runtime, session_id, floating_pane_id).solved_size,
        FloatingPaneSizeSolve::Sized(Size {
            column_count: 72,
            row_count: 24,
        })
    );

    let roomy_events = runtime.handle_client_resize(
        alice_client_id,
        alice_viewport_size,
        Some(PaneArea::Reported(Size {
            column_count: 80,
            row_count: 22,
        })),
        None,
    );

    assert_eq!(
        roomy_events,
        vec![
            build_pty_resized(tiled_pane_id, 78, 20),
            build_pty_resized(floating_pane_id, 46, 9),
        ]
    );
}

#[test]
fn detaching_the_last_client_freezes_every_floating_pane_until_a_client_attaches() {
    let FloatingPaneFixture {
        mut runtime,
        session_id,
        alice_client_id,
        tab_id,
        tiled_pane_id,
        floating_pane_id,
        ..
    } = build_floating_pane_fixture();

    let detach_events = runtime.handle_client_detach(alice_client_id);

    assert_eq!(detach_events, Vec::new());
    assert_eq!(
        get_floating_member(&runtime, session_id, floating_pane_id).solved_size,
        FloatingPaneSizeSolve::Sized(Size {
            column_count: 72,
            row_count: 24,
        })
    );

    let bob_client_id = ClientId::new();
    let attach_events = runtime.handle_client_attach(
        session_id,
        bob_client_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        tab_id,
        None,
        SystemTime::now(),
        false,
    );

    assert_eq!(
        attach_events,
        vec![
            Event::PaneFocused(PaneFocused {
                client_id: bob_client_id,
                tab_id: Some(tab_id),
                pane_id: tiled_pane_id,
                previous_pane_id: None,
            }),
            build_pty_resized(tiled_pane_id, 78, 20),
            build_pty_resized(floating_pane_id, 46, 9),
        ]
    );
}

#[test]
fn a_client_viewing_another_tab_still_shrinks_every_floating_pane() {
    let FloatingPaneFixture {
        mut runtime,
        session_id,
        floating_pane_id,
        ..
    } = build_floating_pane_fixture();
    let (other_tab_id, other_pane_id) = (TabId::new(), PaneId::new());
    let session = runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("the seeded session");
    register_pane_record(session, other_pane_id);
    register_session_tab(session, other_tab_id, other_pane_id);

    // The other tab records no focus, so bob lands on no pane, and its pane
    // has no PTY to resize.
    let attach_events = runtime.handle_client_attach(
        session_id,
        ClientId::new(),
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        other_tab_id,
        None,
        SystemTime::now(),
        false,
    );

    assert_eq!(
        attach_events,
        vec![build_pty_resized(floating_pane_id, 46, 9)]
    );
}

#[test]
fn a_floating_pane_takes_the_cell_size_of_the_earliest_attached_client_on_any_tab() {
    let FloatingPaneFixture {
        mut runtime,
        session_id,
        alice_client_id,
        floating_pane_id,
        ..
    } = build_floating_pane_fixture();
    let read_floating_cell_size = |runtime: &Server| {
        runtime.list_terminal_engines()[&floating_pane_id]
            .get_terminal_state()
            .get_cell_size()
    };
    let (other_tab_id, other_pane_id) = (TabId::new(), PaneId::new());
    let session = runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("the seeded session");
    register_pane_record(session, other_pane_id);
    register_session_tab(session, other_tab_id, other_pane_id);
    let alice_cell_size =
        PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");
    let bob_cell_size =
        PixelCellSize::from_pixel_dimensions(8, 16).expect("positive cell dimensions");
    assert_eq!(read_floating_cell_size(&runtime), None);

    runtime.handle_client_cell_size(alice_client_id, alice_cell_size);
    assert_eq!(read_floating_cell_size(&runtime), Some(alice_cell_size));

    // bob attaches after alice and views another tab.
    runtime.handle_client_attach(
        session_id,
        ClientId::new(),
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        other_tab_id,
        Some(bob_cell_size),
        SystemTime::now(),
        false,
    );
    assert_eq!(read_floating_cell_size(&runtime), Some(alice_cell_size));

    runtime.handle_client_detach(alice_client_id);
    assert_eq!(read_floating_cell_size(&runtime), Some(bob_cell_size));
}

#[test]
fn a_client_moving_to_another_session_regrows_the_floating_panes_it_left() {
    let FloatingPaneFixture {
        mut runtime,
        session_id,
        tab_id,
        tiled_pane_id,
        floating_pane_id,
        ..
    } = build_floating_pane_fixture();
    let bob_client_id = ClientId::new();
    let bob_viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime.handle_client_attach(
        session_id,
        bob_client_id,
        bob_viewport_size,
        None,
        tab_id,
        None,
        SystemTime::now(),
        false,
    );
    let mut other_session = build_bare_session(SessionId::new());
    let (other_tab_id, other_pane_id) = (TabId::new(), PaneId::new());
    register_pane_record(&mut other_session, other_pane_id);
    register_session_tab(&mut other_session, other_tab_id, other_pane_id);
    let other_session_id = other_session.session_id;
    runtime
        .session_by_id
        .insert(other_session_id, other_session);

    let move_events = runtime.handle_client_attach(
        other_session_id,
        bob_client_id,
        bob_viewport_size,
        None,
        other_tab_id,
        None,
        SystemTime::now(),
        false,
    );

    assert_eq!(
        move_events,
        vec![
            build_pty_resized(tiled_pane_id, 118, 38),
            build_pty_resized(floating_pane_id, 70, 20),
        ]
    );
}

#[test]
fn attach_applies_cell_measurement_before_reflow_and_resize_can_clear_it() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let _bootstrap_client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);
    let cell_size = PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");
    let measured_client_id = ClientId::new();

    runtime.handle_client_attach(
        session_id,
        measured_client_id,
        viewport_size,
        None,
        tab_id,
        Some(cell_size),
        SystemTime::now(),
        false,
    );
    let measured_client = runtime.session_by_id[&session_id]
        .clients
        .list_attached_clients()
        .find(|candidate| candidate.get_client_id() == measured_client_id)
        .expect("the measured client attached");
    assert_eq!(measured_client.get_cell_size(), Some(cell_size));
    assert_eq!(
        runtime.session_by_id[&session_id].get_tab_cell_size(tab_id),
        Some(cell_size)
    );

    runtime.handle_client_resize(measured_client_id, viewport_size, None, None);
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(measured_client_id)
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
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (_session_id, _tab_id, _pane_id) = get_only_session_slot(&runtime);
    let cell_size = PixelCellSize::from_pixel_dimensions(10, 20).expect("positive cell dimensions");

    let rendered_at = Instant::now();
    assert!(runtime.render_scheduler.claim_due_render(rendered_at));
    runtime.handle_client_cell_size(client_id, cell_size);

    assert_eq!(
        runtime.render_scheduler.compute_next_wakeup(rendered_at),
        Some(FRAME_INTERVAL_DURATION),
        "the accepted measurement schedules the next frame"
    );
}

#[test]
fn client_resize_updates_full_viewport_and_reflows_middle_pane_region() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let initial_viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let resized_viewport_size = Size {
        column_count: 100,
        row_count: 30,
    };
    let client_id = runtime
        .bootstrap_local(SessionId::new(), initial_viewport_size, SystemTime::now())
        .expect("bootstrap");
    let (_session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);

    let emitted_events = runtime.handle_client_resize(client_id, resized_viewport_size, None, None);
    let expected_pty_size = compute_root_pane_pty_size(
        pane_id,
        compute_default_pane_area_size(resized_viewport_size),
        PaneSizing::default(),
    );

    assert_eq!(
        runtime
            .get_session_for_client(client_id)
            .unwrap()
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_viewport_size(),
        resized_viewport_size
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
        emitted_events,
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

    // The larger client cannot lower the per-axis minimum, so the tab size
    // stays 40x24: no reflow, no resize event.
    let joining_client_id = ClientId::new();
    let emitted_events = runtime.handle_client_attach(
        session_id,
        joining_client_id,
        large_viewport_size,
        None,
        tab_id,
        None,
        SystemTime::now(),
        false,
    );

    // No reflow, but the joining client still lands on the tab's pane.
    assert_eq!(
        emitted_events,
        vec![Event::PaneFocused(PaneFocused {
            client_id: joining_client_id,
            tab_id: Some(tab_id),
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
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let session_id = SessionId::new();
    runtime
        .bootstrap_session(
            session_id,
            "seeded-with-no-client".to_string(),
            viewport_size,
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
    let emitted_events = runtime.handle_client_attach(
        session_id,
        client_id,
        viewport_size,
        None,
        tab_id,
        None,
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
            .get_focused_pane_id(tab_id),
        Some(pane_id)
    );
    assert_eq!(
        emitted_events,
        vec![Event::PaneFocused(PaneFocused {
            client_id,
            tab_id: Some(tab_id),
            pane_id,
            previous_pane_id: None,
        })]
    );
}

#[test]
fn reattaching_keeps_the_pane_the_client_already_focused() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _) = get_only_session_slot(&runtime);

    // Attach once to take the tab's first pane, then point the client's focus
    // at another pane id.
    let client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        client_id,
        viewport_size,
        None,
        tab_id,
        None,
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
    let emitted_events = runtime.handle_client_attach(
        session_id,
        client_id,
        viewport_size,
        None,
        tab_id,
        None,
        SystemTime::now(),
        false,
    );

    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .expect("the client is still attached")
            .get_focused_pane_id(tab_id),
        Some(newly_focused_pane_id),
        "a client that already focused a pane keeps it"
    );
    assert_eq!(emitted_events, Vec::new(), "no focus change is announced");
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
        None,
        SystemTime::now(),
        false,
    );
    let resize_count_before_detach = fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("pane spawned")
        .len();

    // The smaller client leaves: only the 80x24 viewer remains, so the tab grows
    // back and the pane's PTY reflows up.
    let emitted_events = runtime.handle_client_detach(small_client_id);

    let expected_pty_size = compute_root_pane_pty_size(
        pane_id,
        compute_default_pane_area_size(large_viewport_size),
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
        emitted_events,
        vec![Event::PtyResized(PtyResized {
            pane_id,
            pty_size: expected_pty_size,
        })]
    );
}

#[test]
fn client_detach_advances_the_placement_revision_of_each_client_still_viewing_the_tab() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let remaining_client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);
    let leaving_client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        leaving_client_id,
        viewport_size,
        None,
        tab_id,
        None,
        SystemTime::now(),
        false,
    );
    let session_revision_before = runtime.session_by_id[&session_id].get_placement_revision();
    let remaining_client_revision_before = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(remaining_client_id)
        .expect("the remaining client is attached")
        .get_placement_revision();

    runtime.handle_client_detach(leaving_client_id);

    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session.get_placement_revision(),
        session_revision_before + 1
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(remaining_client_id)
            .expect("the remaining client stays attached")
            .get_placement_revision(),
        remaining_client_revision_before + 1
    );
}

#[test]
fn last_client_detach_keeps_pty_sizes_and_emits_no_resize() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);
    let resize_count_before_last_detach = fake_pty_backend
        .list_pane_sizes(pane_id)
        .expect("pane spawned")
        .len();

    // The only viewer leaves: the tab has no viewport, so its PTY keeps its size
    // and no resize event is produced. The pane itself stays alive.
    let emitted_events = runtime.handle_client_detach(client_id);

    assert!(emitted_events.is_empty());
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).unwrap().len(),
        resize_count_before_last_detach
    );
    assert!(runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .is_none());
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(pane_id)
            .map(PaneRecord::get_lifecycle),
        Some(&PaneLifecycle::Running)
    );
}

#[test]
fn a_client_leaving_does_not_end_the_session_by_default() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = runtime
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
    runtime.handle_client_detach(client_id);

    assert!(!runtime.is_quit_requested());
    assert!(!runtime.should_shutdown_immediately);
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(pane_id)
            .map(PaneRecord::get_lifecycle),
        Some(&PaneLifecycle::Running)
    );
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
        None,
        SystemTime::now(),
        false,
    );

    // One of two clients leaves: one is still attached, so nothing quits.
    runtime.handle_client_detach(initial_client_id);

    assert!(!runtime.is_quit_requested());
    assert_eq!(
        runtime.session_by_id[&session_id].clients.count_clients(),
        1
    );
}

#[test]
fn auto_close_ends_the_session_when_the_last_client_leaves() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = runtime
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

    runtime.handle_client_detach(client_id);

    assert!(runtime.is_quit_requested());
    // Teardown keeps the graceful window: `should_shutdown_immediately` stays
    // false.
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
        None,
        SystemTime::now(),
        false,
    );
    assert_eq!(
        runtime.session_by_id[&session_id].clients.count_clients(),
        2
    );

    // `DetachAll` runs the same per-client departure the setting watches, so the
    // pass that removes the last one ends the session.
    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::DetachAll,
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![])
    );

    assert_eq!(
        runtime.session_by_id[&session_id].clients.count_clients(),
        0
    );
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
        None,
        SystemTime::now(),
        false,
    );

    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::DetachAll,
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![])
    );

    assert_eq!(
        runtime.session_by_id[&session_id].clients.count_clients(),
        0
    );
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
    let client_id = runtime
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
            Some(client_id),
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
    let client_id = runtime
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
        build_command_envelope(CommandSource::from_key_binding(client_id), Command::Quit);
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.submit_command(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id].clients.count_clients(),
        0
    );
    assert!(!runtime.is_quit_requested());
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(pane_id)
            .map(PaneRecord::get_lifecycle),
        Some(&PaneLifecycle::Running)
    );
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
        None,
        SystemTime::now(),
        false,
    );

    // One of two clients quits: the other is still attached, so nothing quits.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(initial_client_id),
        Command::Quit,
    );
    assert_eq!(
        get_command_outcome(&runtime.submit_command(command_envelope)),
        Ok(vec![])
    );

    assert!(runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(initial_client_id)
        .is_none());
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(additional_client_id)
            .map(Client::get_active_tab_id),
        Some(tab_id)
    );
    assert!(!runtime.is_quit_requested());
}

#[test]
fn quit_from_the_last_client_ends_the_session_under_auto_close() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = runtime
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
        build_command_envelope(CommandSource::from_key_binding(client_id), Command::Quit);
    assert_eq!(
        get_command_outcome(&runtime.submit_command(command_envelope)),
        Ok(vec![])
    );

    assert_eq!(
        runtime.session_by_id[&session_id].clients.count_clients(),
        0
    );
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

    // A keypress from a client that this runtime does not hold locates no
    // session and ends no session.
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
    assert_eq!(
        runtime.session_by_id[&session_id].clients.count_clients(),
        1
    );
    assert!(!runtime.is_quit_requested());
}

#[test]
fn client_attach_to_an_unknown_session_is_dropped() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let emitted_events = runtime.handle_client_attach(
        SessionId::new(),
        ClientId::new(),
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        TabId::new(),
        None,
        SystemTime::now(),
        false,
    );
    assert!(emitted_events.is_empty());
}

#[test]
fn client_detach_of_an_unknown_client_is_dropped() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let emitted_events = runtime.handle_client_detach(ClientId::new());
    assert!(emitted_events.is_empty());
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
    let emitted_events = runtime.handle_client_attach(
        session_id,
        stranger_client_id,
        viewport_size,
        None,
        TabId::new(),
        None,
        SystemTime::now(),
        false,
    );

    assert!(emitted_events.is_empty());
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
    let first_client_id = runtime
        .bootstrap_local(SessionId::new(), large_viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, first_tab_id, first_pane_id) = get_only_session_slot(&runtime);

    // A second live tab: `NewTab` moves the first client onto it, and leaves
    // `first_tab_id` with no viewer and `first_pane_id` at its bootstrap size.
    assert_eq!(
        get_command_outcome(&runtime.dispatch(build_command_envelope(
            CommandSource::from_key_binding(first_client_id),
            Command::NewTab(NewTabArgs::default()),
        ))),
        Ok(vec![
            "TabCreated",
            "PaneCreated",
            "TabFocused",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let second_tab_id = find_other_tab_id(&runtime, session_id, first_tab_id);

    // Two clients view `first_tab_id`. The smaller one, `third_client_id`,
    // limits `first_pane_id` to 40x24.
    let second_client_id = ClientId::new();
    let third_client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        second_client_id,
        large_viewport_size,
        None,
        first_tab_id,
        None,
        SystemTime::now(),
        false,
    );
    runtime.handle_client_attach(
        session_id,
        third_client_id,
        small_viewport_size,
        None,
        first_tab_id,
        None,
        SystemTime::now(),
        false,
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(first_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        compute_root_pane_pty_size(
            first_pane_id,
            compute_default_pane_area_size(small_viewport_size),
            PaneSizing::default(),
        )
    );
    let resize_count_before_tab_reattach = fake_pty_backend
        .list_pane_sizes(first_pane_id)
        .unwrap()
        .len();

    // `third_client_id` re-attaches onto `second_tab_id` and leaves
    // `first_tab_id`, where only the 80x24 `second_client_id` remains:
    // `first_pane_id` grows back.
    let emitted_events = runtime.handle_client_attach(
        session_id,
        third_client_id,
        large_viewport_size,
        None,
        second_tab_id,
        None,
        SystemTime::now(),
        false,
    );

    let expected_pty_size = compute_root_pane_pty_size(
        first_pane_id,
        compute_default_pane_area_size(large_viewport_size),
        PaneSizing::default(),
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(first_pane_id)
            .unwrap()
            .len(),
        resize_count_before_tab_reattach + 1
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(first_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        expected_pty_size
    );
    // `third_client_id` has no stored focus in `second_tab_id`, so it lands on
    // that tab's pane. The reflow of `first_tab_id` follows.
    let second_pane_id = runtime.session_by_id[&session_id].tabs[&second_tab_id]
        .get_layout_tree()
        .list_leaf_pane_ids()
        .first()
        .copied()
        .expect("the created tab holds one pane");
    assert_eq!(
        emitted_events,
        vec![
            Event::PaneFocused(PaneFocused {
                client_id: third_client_id,
                tab_id: Some(second_tab_id),
                pane_id: second_pane_id,
                previous_pane_id: None,
            }),
            Event::PtyResized(PtyResized {
                pane_id: first_pane_id,
                pty_size: expected_pty_size,
            })
        ]
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(third_client_id)
            .unwrap()
            .get_active_tab_id(),
        second_tab_id
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
        None,
        SystemTime::now(),
        false,
    );

    assert!(runtime.poll_render(now + Duration::from_secs(1)));
}

#[test]
fn client_detach_schedules_a_render() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = runtime
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
    runtime.handle_client_detach(client_id);

    assert!(runtime.poll_render(now + Duration::from_secs(1)));
}

#[test]
fn unviewed_tab_adoption_sizes_the_new_pane_to_the_pane_region() {
    let viewport_size = Size {
        column_count: 100,
        row_count: 40,
    };

    // Baseline: the same-sized client splits the tab it already views, so the
    // solve runs against the tab's drawable pane region.
    let (mut viewed_runtime, viewed_fake_pty_backend, _viewed_runtime_event_sender) =
        build_runtime_with_fake();
    let viewed_client_id = ClientId::new();
    let viewed_tab_id = TabId::new();
    let viewed_root_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, viewed_root_pane_id);
    register_session_tab(&mut session, viewed_tab_id, viewed_root_pane_id);
    let mut client = Client::from_attachment(
        viewed_client_id,
        session.session_id,
        SystemTime::now(),
        viewport_size,
        None,
        viewed_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(viewed_tab_id, viewed_root_pane_id);
    session.attach_client(client);
    let viewed_session_id = session.session_id;
    viewed_runtime
        .session_by_id
        .insert(viewed_session_id, session);
    viewed_runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(viewed_client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let baseline_pane_id =
        find_other_pane_id(&viewed_runtime, viewed_session_id, &[viewed_root_pane_id]);
    let baseline_size = viewed_fake_pty_backend
        .list_pane_sizes(baseline_pane_id)
        .expect("baseline pane spawned")[0];

    // Adoption: an identical client is designated onto an UNVIEWED tab. The
    // new pane must be fit and spawned against the client's pane region — the
    // same geometry as the viewed baseline — not the full terminal viewport.
    let (mut adopting_runtime, adopting_fake_pty_backend, _adopting_runtime_event_sender) =
        build_runtime_with_fake();
    let adopting_client_id = ClientId::new();
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
        adopting_client_id,
        session.session_id,
        SystemTime::now(),
        viewport_size,
        None,
        front_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(front_tab_id, front_pane_id);
    session.attach_client(client);
    let adopting_session_id = session.session_id;
    adopting_runtime
        .session_by_id
        .insert(adopting_session_id, session);
    adopting_runtime.dispatch(build_command_envelope(
        CommandSource::from_external_cli(Some(adopting_session_id), None),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(back_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            client_id: Some(adopting_client_id),
            ..build_new_pane_args()
        }),
    ));
    let adopted_pane_id = find_other_pane_id(
        &adopting_runtime,
        adopting_session_id,
        &[front_pane_id, back_pane_id],
    );
    let adopted_size = adopting_fake_pty_backend
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
            placement: NewPanePlacement::Split {
                source_pane_id: Some(pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            ..build_new_pane_args()
        }),
    ));
    assert_eq!(
        get_command_outcome(&command_result),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PtyResized",
            "PtyResized"
        ])
    );

    assert!(runtime.poll_render(now + Duration::from_secs(1)));
}

#[test]
fn same_session_reattach_preserves_client_view_state() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let initial_viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let client_id = runtime
        .bootstrap_local(SessionId::new(), initial_viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, pane_id) = get_only_session_slot(&runtime);

    // The client accumulated per-tab focus.
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .clients
        .get_client_mut_by_id(client_id)
        .unwrap()
        .update_focused_pane(tab_id, pane_id);

    // A re-attach of the same live client id updates its record in place: the
    // focus stays, and the viewport takes the new size.
    let grown_viewport_size = Size {
        column_count: 100,
        row_count: 30,
    };
    runtime.handle_client_attach(
        session_id,
        client_id,
        grown_viewport_size,
        None,
        tab_id,
        None,
        SystemTime::now(),
        false,
    );

    let client_record = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("still attached");
    assert_eq!(client_record.get_focused_pane_id(tab_id), Some(pane_id));
    assert_eq!(client_record.get_viewport_size(), grown_viewport_size);
    assert_eq!(client_record.get_active_tab_id(), tab_id);
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

    let client_id = runtime
        .bootstrap_local(SessionId::new(), large_viewport_size, SystemTime::now())
        .expect("first session");
    let (first_session_id, _first_tab_id, first_pane_id) = get_only_session_slot(&runtime);

    // A second, independent session with its own live pane.
    runtime
        .bootstrap_local(SessionId::new(), large_viewport_size, SystemTime::now())
        .expect("second session");
    let second_session_id = *runtime
        .session_by_id
        .keys()
        .find(|&&candidate_session_id| candidate_session_id != first_session_id)
        .expect("the second session");
    let second_session = &runtime.session_by_id[&second_session_id];
    let second_tab_id = *second_session.tabs.keys().next().expect("its tab");
    let second_pane_id = second_session
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .next()
        .expect("its pane");
    let pane_1_resizes_before = fake_pty_backend
        .list_pane_sizes(first_pane_id)
        .expect("pane spawned")
        .len();

    // Move `client` from session 1 into session 2 at a smaller viewport.
    let emitted_events = runtime.handle_client_attach(
        second_session_id,
        client_id,
        small_viewport_size,
        None,
        second_tab_id,
        None,
        SystemTime::now(),
        false,
    );

    // It left session 1 entirely and is now the 40x24 co-viewer of session 2.
    assert!(runtime.session_by_id[&first_session_id]
        .clients
        .get_client_by_id(client_id)
        .is_none());
    assert_eq!(
        runtime.session_by_id[&second_session_id]
            .clients
            .get_client_by_id(client_id)
            .expect("moved into session 2")
            .get_active_tab_id(),
        second_tab_id
    );

    // Session 2's pane shrinks to the new minimum; session 1's pane keeps its
    // size (its tab lost its only viewer).
    let expected_pty_size = compute_root_pane_pty_size(
        second_pane_id,
        compute_default_pane_area_size(small_viewport_size),
        PaneSizing::default(),
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(second_pane_id)
            .unwrap()
            .last()
            .unwrap(),
        expected_pty_size
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(first_pane_id)
            .unwrap()
            .len(),
        pane_1_resizes_before
    );
    // The client had focused nothing in session 2, so it lands on that tab's
    // pane before the reflow its smaller viewport caused.
    assert_eq!(
        emitted_events,
        vec![
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(second_tab_id),
                pane_id: second_pane_id,
                previous_pane_id: None,
            }),
            Event::PtyResized(PtyResized {
                pane_id: second_pane_id,
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
    let first_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, tab_id, first_pane_id);
    let client_id = ClientId::new();
    attach_client(&mut session, client_id, tab_id, Some(first_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    // First split: pane_x gets a live PTY + engine at the two-pane width.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let split_pane_id = find_other_pane_id(&runtime, session_id, &[first_pane_id]);
    let wide_pane_pty_size = runtime.pty_size_by_pane_id[&split_pane_id];
    let written_line: String = "A".repeat(wide_pane_pty_size.column_count as usize - 2);
    let _ = runtime
        .terminal_engine_by_pane_id
        .get_mut(&split_pane_id)
        .unwrap()
        .process_pty_output(written_line.as_bytes());

    // Second split of pane_x (it holds focus): pane_x narrows.
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let narrow_pane_pty_size = runtime.pty_size_by_pane_id[&split_pane_id];
    assert!(
        narrow_pane_pty_size.column_count < wide_pane_pty_size.column_count,
        "narrow {narrow_pane_pty_size:?} wide {wide_pane_pty_size:?}"
    );
    let grid = runtime.terminal_engine_by_pane_id[&split_pane_id]
        .get_terminal_state()
        .get_active_grid();
    assert_eq!(
        grid.get_grid_dimensions(),
        (
            narrow_pane_pty_size.row_count,
            narrow_pane_pty_size.column_count
        )
    );
    let first_row_text: String = grid.list_rows()[0]
        .iter()
        .map(koshi_terminal::grid::state::Cell::get_character)
        .collect();
    let second_row_text: String = grid.list_rows()[1]
        .iter()
        .map(koshi_terminal::grid::state::Cell::get_character)
        .collect();
    let expected_first_row_text = "A".repeat(narrow_pane_pty_size.column_count as usize);
    let wrapped_remainder_count =
        wide_pane_pty_size.column_count as usize - 2 - narrow_pane_pty_size.column_count as usize;
    let expected_second_row_text = format!(
        "{}{}",
        "A".repeat(wrapped_remainder_count),
        " ".repeat(narrow_pane_pty_size.column_count as usize - wrapped_remainder_count)
    );
    assert_eq!(
        first_row_text, expected_first_row_text,
        "the first row holds a full wrapped slice"
    );
    assert_eq!(
        second_row_text, expected_second_row_text,
        "the second row holds the wrapped remainder"
    );
}

// The genesis root pane goes through `bootstrap_local`, not the new-pane
// handler; its first split must still re-wrap the root's grid content.
#[test]
fn bootstrap_root_pane_rewraps_on_first_split() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::now(),
        )
        .expect("bootstrap");
    let (_, _, root_pane_id) = get_only_session_slot(&runtime);
    let wide_pane_pty_size = runtime.pty_size_by_pane_id[&root_pane_id];
    let written_line: String = "A".repeat(wide_pane_pty_size.column_count as usize - 2);
    let _ = runtime
        .terminal_engine_by_pane_id
        .get_mut(&root_pane_id)
        .unwrap()
        .process_pty_output(written_line.as_bytes());

    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
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
    let first_row_text: String = grid.list_rows()[0]
        .iter()
        .map(koshi_terminal::grid::state::Cell::get_character)
        .collect();
    assert_eq!(
        first_row_text,
        "A".repeat(narrow_pane_pty_size.column_count as usize)
    );
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
    let tab_size = Size {
        column_count: 80,
        row_count: 24,
    };

    // Each pane is sized to its 40-column half minus its one-cell border on
    // each side (38 content columns, 22 rows), not the whole 80-column tab.
    let sizes = compute_pane_spawn_sizes(&tree, tab_size, PaneSizing::default());
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

    // A single pane over the same tab size keeps the full inner width, so the
    // two-pane tiles really are narrower.
    assert_eq!(
        compute_root_pane_pty_size(left_pane_id, tab_size, PaneSizing::default()),
        PtySize {
            column_count: 78,
            row_count: 22
        }
    );
}

#[test]
fn compute_root_pane_pty_size_falls_back_to_the_whole_viewport_for_a_suppressed_pane() {
    // A 3x3 viewport is below the pane's border-inclusive floor of 4 columns,
    // so the solve gives the pane no content rect and the size is taken from
    // the whole viewport rect instead.
    assert_eq!(
        compute_root_pane_pty_size(
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
fn tab_focused_in_reports_the_first_focused_tab() {
    let (first_tab_id, second_tab_id) = (TabId::new(), TabId::new());
    let client_id = ClientId::new();
    let build_tab_focused_event = |tab_id| {
        Event::TabFocused(koshi_core::event::TabFocused {
            client_id,
            tab_id,
            previous_tab_id: first_tab_id,
        })
    };

    // Two switches in one batch: the first entry names the answer.
    assert_eq!(
        find_first_focused_tab_id(&[
            build_tab_focused_event(first_tab_id),
            build_tab_focused_event(second_tab_id)
        ]),
        Some(first_tab_id)
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

    let spawn_spec = runtime.build_default_shell_spec(None, BTreeMap::new());
    assert_eq!(spawn_spec.program, PathBuf::from("/opt/homebrew/bin/fish"));
    assert_eq!(spawn_spec.shell_kind, ShellKind::Fish);
    assert_eq!(
        spawn_spec
            .environment_variables
            .get("TERM")
            .map(String::as_str),
        Some("xterm-kitty")
    );
    assert_eq!(
        spawn_spec
            .environment_variables
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
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap");
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    runtime.config.pane.gap_cell_count = 2;

    let render_snapshot = runtime.build_snapshot(client_id).expect("a snapshot");

    assert_eq!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .gap_cell_count,
        2
    );
    let pane_slots = &render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots;
    assert_eq!(pane_slots.len(), 2);
    assert_eq!(
        pane_slots[1].outer_rect.origin.column,
        pane_slots[0].outer_rect.origin.column + pane_slots[0].outer_rect.size.column_count + 2
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );
    let exiting_pane_id = find_other_pane_id(&runtime, session_id, &[root_pane_id]);
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized",
            "PtyResized"
        ])
    );
    // The pane that is neither the root nor the first split.
    let surviving_pane_id =
        find_other_pane_id(&runtime, session_id, &[root_pane_id, exiting_pane_id]);

    // The first exit removes `exiting_pane_id`.
    let first_exit_events = runtime.handle_child_exit(exiting_pane_id, ExitStatus::ExitCode(0));
    assert_eq!(
        list_event_names(&first_exit_events),
        [
            "PaneProcessExited",
            "PaneClosing",
            "PaneRemoved",
            "LayoutChanged",
            "PtyResized"
        ]
    );
    assert!(runtime.session_by_id[&session_id]
        .panes
        .get_pane_record_by_id(exiting_pane_id)
        .is_none());
    let surviving_pane_resize_count = fake_pty_backend
        .list_pane_sizes(surviving_pane_id)
        .expect("the surviving pane spawned")
        .len();

    // A second exit for the removed pane is dropped.
    let duplicate_exit_events = runtime.handle_child_exit(exiting_pane_id, ExitStatus::ExitCode(0));
    assert!(
        duplicate_exit_events.is_empty(),
        "a duplicate exit emits nothing"
    );

    // The surviving pane keeps its record, its live entry, its PTY size, and its
    // engine, and the dropped exit resizes nothing.
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(surviving_pane_id)
            .map(PaneRecord::get_lifecycle),
        Some(&PaneLifecycle::Running)
    );
    assert!(runtime.live_pane_ids.contains(&surviving_pane_id));
    assert!(runtime.pty_size_by_pane_id.contains_key(&surviving_pane_id));
    assert!(runtime
        .terminal_engine_by_pane_id
        .contains_key(&surviving_pane_id));
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(surviving_pane_id)
            .expect("the surviving pane spawned")
            .len(),
        surviving_pane_resize_count,
        "the dropped duplicate reflowed nothing"
    );
}

#[test]
fn output_arriving_after_a_child_exit_is_dropped_and_a_live_pane_still_updates() {
    // Output arrives for a pane whose child already exited, and for a live pane.
    // The exited pane's output is dropped and changes no state. The live pane's
    // output reaches its engine.
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

    for expected_event_names in [
        vec!["PaneCreated", "LayoutChanged", "PaneFocused", "PtyResized"],
        vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized",
            "PtyResized",
        ],
    ] {
        let command_envelope = build_command_envelope(
            CommandSource::from_key_binding(client_id),
            Command::NewPane(build_new_pane_args()),
        );
        assert_eq!(
            get_command_outcome(&runtime.dispatch(command_envelope)),
            Ok(expected_event_names)
        );
    }
    // The two spawned panes are exactly the ones with an engine; the root has none.
    let engine_pane_ids: Vec<PaneId> = runtime.list_terminal_engines().keys().copied().collect();
    assert_eq!(engine_pane_ids.len(), 2, "two spawned panes hold engines");
    let (exited_pane_id, live_pane_id) = (engine_pane_ids[0], engine_pane_ids[1]);

    // The exit removes the pane and its engine.
    let _ = runtime.handle_child_exit(exited_pane_id, ExitStatus::ExitCode(0));
    assert!(!runtime
        .list_terminal_engines()
        .contains_key(&exited_pane_id));

    // Late output for the now-engineless pane is a no-op.
    runtime.handle_pty_output(exited_pane_id, b"late");
    assert!(!runtime
        .list_terminal_engines()
        .contains_key(&exited_pane_id));

    // The live sibling still parses its output: two printable bytes advance its
    // cursor to column 2.
    runtime.handle_pty_output(live_pane_id, b"hi");
    let (row_index, column_index) = runtime
        .list_terminal_engines()
        .get(&live_pane_id)
        .expect("the live pane keeps its engine")
        .get_terminal_state()
        .get_active_cursor_position();
    assert_eq!((row_index, column_index), (0, 2));
}

#[test]
fn a_rejected_command_leaves_state_intact_and_the_next_command_works() {
    // A rejection must not be a dead end: after one command bounces off validation
    // the runtime keeps every bit of state and accepts the next command normally.
    let (mut runtime, _runtime_event_sender, client_id, session_id) = build_lock_fixture();

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
        get_client_lock_mode(&runtime, session_id, client_id),
        LockMode::Normal
    );

    // The very next command lands.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["InputModeChanged"])
    );
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, client_id),
        LockMode::Locked
    );
}

#[test]
fn a_command_after_quit_still_dispatches() {
    // `Quit` sets the loop's exit flags but does not itself gate dispatch — the
    // loop exits by polling `quit_requested`, not by dispatch refusing commands.
    // Pin that: a command issued after Quit, before the loop notices, still runs.
    let (mut runtime, _runtime_event_sender, client_id, session_id) = build_lock_fixture();

    assert_eq!(
        get_command_outcome(
            &runtime.dispatch(build_sessionless_cli_command_envelope(Command::Quit))
        ),
        Ok(vec![])
    );
    assert!(runtime.is_quit_requested());

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["InputModeChanged"])
    );
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, client_id),
        LockMode::Locked
    );
}

// A rejected command writes one warning line that names the command and the
// reject reason.
#[test]
fn a_rejected_command_writes_a_warning_naming_the_reason() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let (_subscriber_guard, captured_logs) = koshi_observability::logging::with_test_writer();

    // The command names a client that is attached to no session: validation
    // rejects the source before it resolves the tab.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(ClientId::new()),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(TabId::new()),
            client_id: None,
        }),
    );
    let command_id = command_envelope.command_id;
    let command_result = runtime.dispatch(command_envelope);

    assert_eq!(
        command_result,
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::SourceClientStale,
            help: None,
        }
    );
    let log_text = captured_logs.contents();
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
    let (mut runtime, _runtime_event_sender, client_id, _session_id) = build_lock_fixture();
    let (_subscriber_guard, captured_logs) = koshi_observability::logging::with_test_writer();

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["InputModeChanged"])
    );

    let log_text = captured_logs.contents();
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

/// A session with one tab, one pane, and no clients. Returns the runtime, the
/// session, the tab, and the pane.
fn build_acting_client_fixture() -> (Server, SessionId, TabId, PaneId) {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, pane_id);
    register_session_tab(&mut session, tab_id, pane_id);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    (runtime, session_id, tab_id, pane_id)
}

#[test]
fn lock_from_a_pane_whose_client_detached_locks_the_sole_client() {
    let (mut runtime, session_id, tab_id, pane_id) = build_acting_client_fixture();
    let attached_client_id = ClientId::new();
    let detached_client_id = ClientId::new();
    let session = runtime.session_by_id.get_mut(&session_id).expect("session");
    attach_client(session, attached_client_id, tab_id, Some(pane_id));

    // The pane's own client is detached, and exactly one client is attached:
    // `koshi lock` acts on that client.
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["InputModeChanged"])
    );
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, attached_client_id),
        LockMode::Locked
    );
}

#[test]
fn lock_from_a_clientless_pane_locks_the_sole_client() {
    let (mut runtime, session_id, tab_id, pane_id) = build_acting_client_fixture();
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["InputModeChanged"])
    );
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, attached_client_id),
        LockMode::Locked
    );
}

#[test]
fn lock_from_a_detached_client_with_two_attached_is_ambiguous() {
    let (mut runtime, session_id, tab_id, pane_id) = build_acting_client_fixture();
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
        get_client_lock_mode(&runtime, session_id, first_attached_client_id),
        LockMode::Normal
    );
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, second_attached_client_id),
        LockMode::Normal
    );
}

#[test]
fn lock_from_an_attached_client_ignores_the_sole_client_fallback() {
    let (mut runtime, session_id, tab_id, pane_id) = build_acting_client_fixture();
    let other_client_id = ClientId::new();
    let issuer_client_id = ClientId::new();
    let session = runtime.session_by_id.get_mut(&session_id).expect("session");
    // The issuer attaches after `other_client_id`.
    attach_client(session, other_client_id, tab_id, Some(pane_id));
    attach_client(session, issuer_client_id, tab_id, Some(pane_id));

    // The issuer is attached: the lock acts on the issuer, with two clients
    // attached.
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["InputModeChanged"])
    );
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, issuer_client_id),
        LockMode::Locked
    );
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, other_client_id),
        LockMode::Normal
    );
}

#[test]
fn fullscreen_from_a_clientless_pane_zooms_the_sole_client() {
    let ResizeFixture {
        mut runtime,
        session_id,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    let tab_id = get_only_tab_id(&runtime, session_id);

    // The zoom is per-client state; with one client attached, the pane the CLI
    // was issued from fills that client's view.
    let command_source =
        CommandSource::from_in_session_cli(session_id, None, split_pane_id, PathBuf::from("/sock"));
    let command_envelope = build_command_envelope(command_source, Command::TogglePaneFullscreen);
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged", "PtyResized"])
    );
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, client_id, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: split_pane_id
        }
    );
}

/// `is_client_scoped` answers `true` for the command whose target is only the
/// issuing client's view. Every other variant carries a target of its own,
/// resolved by its own resolver. Every command [`build_every_command`] lists is
/// classified here.
#[test]
fn client_scoped_is_exactly_toggle_mouse_select() {
    let cases = build_every_command(TabId::new(), PaneId::new());

    assert_eq!(cases.len(), COMMAND_VARIANT_COUNT);
    for command in &cases {
        assert_eq!(
            Server::is_client_scoped(command),
            matches!(command, Command::ToggleMouseSelect),
            "{command:?}"
        );
    }
}

/// A session with two attached clients. Each client views a tab of its own,
/// with that tab's one pane focused.
struct TwoClientFixture {
    runtime: Server,
    session_id: SessionId,
    /// The client that attached second.
    second_client_id: ClientId,
    /// The tab `second_client_id` views.
    second_tab_id: TabId,
    /// The pane `second_client_id` has focused in `second_tab_id`.
    second_pane_id: PaneId,
}

/// Build a [`TwoClientFixture`]: the first client views the first tab, and
/// the second client views the second tab.
fn build_two_client_fixture() -> TwoClientFixture {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let (first_client_id, second_client_id) = (ClientId::new(), ClientId::new());
    let (first_tab_id, second_tab_id) = (TabId::new(), TabId::new());
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, first_pane_id);
    register_session_tab(&mut session, first_tab_id, first_pane_id);
    register_pane_record(&mut session, second_pane_id);
    register_session_tab(&mut session, second_tab_id, second_pane_id);
    attach_client(
        &mut session,
        first_client_id,
        first_tab_id,
        Some(first_pane_id),
    );
    attach_client(
        &mut session,
        second_client_id,
        second_tab_id,
        Some(second_pane_id),
    );
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    TwoClientFixture {
        runtime,
        session_id,
        second_client_id,
        second_tab_id,
        second_pane_id,
    }
}

#[test]
fn an_explicit_target_client_zooms_that_client() {
    let TwoClientFixture {
        runtime,
        session_id,
        second_client_id,
        second_tab_id,
        second_pane_id,
    } = build_two_client_fixture();

    // The named client decides both halves: its own view flips, and the pane is
    // the one it has focused in the tab it is looking at.
    let command_source = CommandSource::from_external_cli(Some(session_id), Some(second_client_id));
    let fullscreen_target = runtime
        .resolve_fullscreen_target(&command_source, Some(&runtime.session_by_id[&session_id]))
        .ok()
        .expect("the named client is attached");

    assert_eq!(fullscreen_target.client_id, second_client_id);
    assert_eq!(fullscreen_target.tab_id, second_tab_id);
    assert_eq!(fullscreen_target.pane_id, second_pane_id);
}

#[test]
fn a_target_client_in_another_session_is_not_found() {
    let TwoClientFixture {
        mut runtime,
        session_id,
        ..
    } = build_two_client_fixture();
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
    let TwoClientFixture {
        runtime,
        session_id,
        ..
    } = build_two_client_fixture();

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
    let (mut runtime, session_id, tab_id, pane_id) = build_acting_client_fixture();
    let first_client_id = ClientId::new();
    attach_client(
        runtime.session_by_id.get_mut(&session_id).expect("session"),
        first_client_id,
        tab_id,
        Some(pane_id),
    );

    // One client is attached: the command acts on its view.
    let command_source = CommandSource::from_external_cli(Some(session_id), None);
    let fullscreen_target = runtime
        .resolve_fullscreen_target(&command_source, Some(&runtime.session_by_id[&session_id]))
        .ok()
        .expect("the sole attached client stands in");

    assert_eq!(fullscreen_target.client_id, first_client_id);
}

#[test]
fn no_flag_with_no_client_is_a_stale_source() {
    let (runtime, session_id, _tab_id, _pane_id) = build_acting_client_fixture();

    // Nobody is attached, so there is no view to flip at all.
    let command_source = CommandSource::from_external_cli(Some(session_id), None);
    let rejection = runtime
        .resolve_fullscreen_target(&command_source, Some(&runtime.session_by_id[&session_id]))
        .err()
        .expect("no attached client is no target");

    assert_eq!(rejection.reason, RejectReason::SourceClientStale);
}

/// Two clients share one tab, and `--client` names the second client. The
/// second client's focused pane fills its screen, and the first client keeps
/// its tiled view.
#[test]
fn a_named_client_zooms_only_its_own_view() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let (first_client_id, second_client_id) = (ClientId::new(), ClientId::new());
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
    attach_client(&mut session, first_client_id, tab_id, Some(focused_pane_id));
    attach_client(&mut session, second_client_id, tab_id, Some(other_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), Some(second_client_id)),
        Command::TogglePaneFullscreen,
    );
    let command_id = command_envelope.command_id;
    match runtime.dispatch(command_envelope) {
        CommandResult::Ok {
            command_id: ok_command_id,
            ..
        } => assert_eq!(ok_command_id, command_id),
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, second_client_id, tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: other_pane_id
        }
    );
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, first_client_id, tab_id),
        LayoutMode::Tiled
    );
}

/// Naming the same client twice flips its view back. The client that was never
/// named stays tiled through both halves.
#[test]
fn naming_the_same_client_twice_returns_that_client_to_tiled() {
    let (mut runtime, _runtime_event_sender) = build_runtime();
    let (first_client_id, second_client_id) = (ClientId::new(), ClientId::new());
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
    attach_client(&mut session, first_client_id, tab_id, Some(first_pane_id));
    attach_client(&mut session, second_client_id, tab_id, Some(second_pane_id));
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);

    let command_source = CommandSource::from_external_cli(Some(session_id), Some(second_client_id));
    for _ in 0..2 {
        let command_envelope =
            build_command_envelope(command_source.clone(), Command::TogglePaneFullscreen);
        let command_id = command_envelope.command_id;
        match runtime.dispatch(command_envelope) {
            CommandResult::Ok {
                command_id: ok_command_id,
                ..
            } => assert_eq!(ok_command_id, command_id),
            unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
        }
    }

    assert_eq!(
        get_client_layout_mode(&runtime, session_id, second_client_id, tab_id),
        LayoutMode::Tiled
    );
    assert_eq!(
        get_client_layout_mode(&runtime, session_id, first_client_id, tab_id),
        LayoutMode::Tiled
    );
}

#[test]
fn focus_tab_from_a_detached_client_falls_back_to_the_sole_client() {
    let (mut runtime, session_id, tab_id, pane_id) = build_acting_client_fixture();
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["TabFocused", "PaneFocused"])
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(attached_client_id)
            .expect("client")
            .get_active_tab_id(),
        second_tab_id
    );
}

#[test]
fn an_explicit_client_outranks_the_sole_client_fallback() {
    let (mut runtime, session_id, tab_id, pane_id) = build_acting_client_fixture();
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["TabFocused", "PaneFocused"])
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(named_client_id)
            .expect("client")
            .get_active_tab_id(),
        second_tab_id
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(issuer_client_id)
            .expect("client")
            .get_active_tab_id(),
        tab_id,
        "the issuing client's own view does not move"
    );
}

#[test]
fn an_explicit_client_that_is_not_attached_never_falls_back() {
    let (mut runtime, session_id, tab_id, pane_id) = build_acting_client_fixture();
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
            .get_active_tab_id(),
        tab_id
    );
}

#[test]
fn fullscreen_from_a_pane_on_a_tab_nobody_views_is_refused() {
    let (mut runtime, session_id, tab_id, pane_id) = build_acting_client_fixture();
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
    let (mut runtime, session_id, tab_id, pane_id) = build_acting_client_fixture();
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["InputModeChanged"])
    );
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, attached_client_id),
        LockMode::Locked
    );
}

// --- External targeting: acting-client defaults, tab-anchored new-pane,
// --- explicit lock client ---

#[test]
fn external_pane_default_acts_on_the_sole_clients_focused_pane() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        split_pane_id,
        split_pane_pty_size,
        ..
    } = build_resize_fixture();

    // No `--pane`: the external command acts on the focused pane of the sole
    // attached client, `split_pane_id`. Growing its left border by 5 takes 5
    // columns from the root pane.
    let command_source = CommandSource::from_external_cli(Some(session_id), None);
    let command_envelope = build_command_envelope(
        command_source,
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction: Direction::Left,
            resize_amount_cells: 5,
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["LayoutChanged", "PtyResized"])
    );

    // The resize lands on `split_pane_id` alone: its PTY grows by 5 columns.
    let expected_pty_size = PtySize {
        column_count: split_pane_pty_size.column_count + 5,
        row_count: split_pane_pty_size.row_count,
    };
    assert_eq!(
        runtime.pty_size_by_pane_id[&split_pane_id],
        expected_pty_size
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(split_pane_id)
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

    // Two clients are attached, each with its own focused pane: the command is
    // rejected as ambiguous.
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["TabMoved"])
    );

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
            placement: NewPanePlacement::Split {
                source_pane_id: None,
                tab_id: Some(back_tab_id),
                direction: Direction::Right,
            },
            working_directory: None,
            spawn_spec: None,
            client_id: None,
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "TabFocused",
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );

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
    assert_eq!(client.get_active_tab_id(), back_tab_id);
    assert_eq!(client.get_focused_pane_id(back_tab_id), Some(new_pane_id));
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
            placement: NewPanePlacement::Split {
                source_pane_id: None,
                tab_id: Some(back_tab_id),
                direction: Direction::Right,
            },
            working_directory: None,
            spawn_spec: None,
            client_id: None,
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "TabFocused",
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );

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
            placement: NewPanePlacement::Split {
                source_pane_id: None,
                tab_id: Some(TabId::new()),
                direction: Direction::Right,
            },
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["InputModeChanged"])
    );
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, target_client_id),
        LockMode::Locked
    );
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, issuer_client_id),
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["InputModeChanged"])
    );
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, target_client_id),
        LockMode::Locked
    );
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, issuer_client_id),
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
        get_client_lock_mode(&runtime, session_id, issuer_client_id),
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec!["InputModeChanged"])
    );
    assert_eq!(
        get_client_lock_mode(&runtime, session_id, client_id),
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
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    // The shell in the focused pane reports its directory over OSC 7.
    runtime.handle_pty_output(split_pane_id, b"\x1b]7;file:///tmp/reported\x07");

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized",
            "PtyResized"
        ])
    );

    assert_eq!(
        get_last_spawn_working_directory(&fake_pty_backend),
        Some(PathBuf::from("/tmp/reported"))
    );
}

#[test]
fn the_shells_report_wins_over_the_os_answer() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    runtime.handle_pty_output(split_pane_id, b"\x1b]7;file:///tmp/reported\x07");
    fake_pty_backend.set_live_working_directory(split_pane_id, "/tmp/os-answer");

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized",
            "PtyResized"
        ])
    );

    assert_eq!(
        get_last_spawn_working_directory(&fake_pty_backend),
        Some(PathBuf::from("/tmp/reported"))
    );
}

#[test]
fn a_remote_shells_reported_directory_is_not_inherited() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    // A shell over SSH reports a directory on another machine; the OS's
    // answer for the local child (the ssh process) is used instead.
    runtime.handle_pty_output(split_pane_id, b"\x1b]7;file://build-server/srv/remote\x07");
    fake_pty_backend.set_live_working_directory(split_pane_id, "/tmp/os-answer");

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized",
            "PtyResized"
        ])
    );

    assert_eq!(
        get_last_spawn_working_directory(&fake_pty_backend),
        Some(PathBuf::from("/tmp/os-answer"))
    );
}

#[test]
fn new_pane_with_no_working_directory_falls_back_to_the_os_answer() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    // No OSC 7 report; the OS knows where the child currently is.
    fake_pty_backend.set_live_working_directory(split_pane_id, "/tmp/os-answer");

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized",
            "PtyResized"
        ])
    );

    assert_eq!(
        get_last_spawn_working_directory(&fake_pty_backend),
        Some(PathBuf::from("/tmp/os-answer"))
    );
}

#[test]
fn new_pane_with_no_working_directory_falls_back_to_the_source_panes_spawn_directory() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        client_id,
        ..
    } = build_resize_fixture();
    // Split a pane into a known directory; it takes focus.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            working_directory: Some(PathBuf::from("/srv/spawned")),
            ..build_new_pane_args()
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized",
            "PtyResized"
        ])
    );

    // No OSC 7 report and no OS answer: the split inherits the working directory
    // the focused pane was spawned in.
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized",
            "PtyResized"
        ])
    );

    assert_eq!(
        get_last_spawn_working_directory(&fake_pty_backend),
        Some(PathBuf::from("/srv/spawned"))
    );
}

#[test]
fn an_explicit_working_directory_wins_over_the_source_panes_directory() {
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    runtime.handle_pty_output(split_pane_id, b"\x1b]7;file:///tmp/reported\x07");

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            working_directory: Some(PathBuf::from("/explicit")),
            ..build_new_pane_args()
        }),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized",
            "PtyResized"
        ])
    );

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
    let ResizeFixture {
        mut runtime,
        fake_pty_backend,
        client_id,
        split_pane_id,
        ..
    } = build_resize_fixture();
    runtime.handle_pty_output(split_pane_id, b"\x1b]7;file:///tmp/reported\x07");

    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "TabCreated",
            "PaneCreated",
            "TabFocused",
            "PaneFocused",
            "PtyResized"
        ])
    );

    assert_eq!(
        get_last_spawn_working_directory(&fake_pty_backend),
        Some(PathBuf::from("/tmp/reported"))
    );
}

#[test]
fn detaching_the_last_client_leaves_the_session_running_with_no_clients() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = runtime
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
    let delivery_receiver = runtime.subscribe(client_id);

    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::Detach(DetachArgs {
            client_id: Some(client_id),
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
    assert_eq!(
        runtime.session_by_id[&session_id].clients.count_clients(),
        0
    );
    assert_eq!(runtime.event_bus.count_subscribers(), 0);

    // The session outlives its last client: the pane is still registered and
    // still holds its PTY.
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(pane_id)
            .map(PaneRecord::get_pane_id),
        Some(pane_id)
    );
    assert!(runtime.live_pane_ids.contains(&pane_id));
    drop(delivery_receiver);
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
        None,
        SystemTime::now(),
        false,
    );
    let initial_client_delivery_receiver = runtime.subscribe(initial_client_id);
    let additional_client_delivery_receiver = runtime.subscribe(additional_client_id);

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

    assert_eq!(
        runtime.session_by_id[&session_id].clients.count_clients(),
        0
    );
    assert!(runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(initial_client_id)
        .is_none());
    assert!(runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(additional_client_id)
        .is_none());

    // Both subscriptions go with the records, and the session's pane lives on.
    assert_eq!(runtime.event_bus.count_subscribers(), 0);
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(pane_id)
            .map(PaneRecord::get_pane_id),
        Some(pane_id)
    );
    drop(initial_client_delivery_receiver);
    drop(additional_client_delivery_receiver);
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
        None,
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
    assert_eq!(
        runtime.session_by_id[&session_id].clients.count_clients(),
        2
    );
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
    let delivery_receiver = runtime.subscribe(only_client_id);

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
    assert_eq!(
        runtime.session_by_id[&session_id].clients.count_clients(),
        0
    );
    assert!(runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(only_client_id)
        .is_none());
    assert_eq!(runtime.event_bus.count_subscribers(), 0);

    // The session model lives on: the pane is still registered and still holds
    // its PTY.
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .get_pane_record_by_id(pane_id)
            .map(PaneRecord::get_pane_id),
        Some(pane_id)
    );
    assert!(runtime.live_pane_ids.contains(&pane_id));
    drop(delivery_receiver);
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
    assert_eq!(
        runtime.session_by_id[&session_id].clients.count_clients(),
        0
    );
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
    let first_session_id = SessionId::new();
    let second_session_id = SessionId::new();
    let first_client_id = ClientId::new();
    let second_client_id = ClientId::new();

    for (session_id, client_id) in [
        (first_session_id, first_client_id),
        (second_session_id, second_client_id),
    ] {
        let mut session = build_bare_session(session_id);
        let pane_id = PaneId::new();
        let tab_id = TabId::new();
        register_pane_record(&mut session, pane_id);
        register_session_tab(&mut session, tab_id, pane_id);
        attach_client(&mut session, client_id, tab_id, None);
        runtime.session_by_id.insert(session_id, session);
    }

    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(first_session_id), None),
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
        runtime.session_by_id[&first_session_id]
            .clients
            .count_clients(),
        0
    );
    assert_eq!(
        runtime.session_by_id[&second_session_id]
            .clients
            .get_client_by_id(second_client_id)
            .map(Client::get_client_id),
        Some(second_client_id)
    );
}

#[test]
fn a_switch_puts_the_session_to_join_on_the_clients_queue() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = runtime
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
    let delivery_receiver = runtime.subscribe(client_id);
    let target_session_id = SessionId::new();

    let command_envelope = build_command_envelope(
        CommandSource::from_in_session_cli(
            session_id,
            Some(client_id),
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

    let session_switch_targets: Vec<SessionId> = delivery_receiver
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
    let client_id = runtime
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
    let delivery_receiver = runtime.subscribe(client_id);

    let command_envelope = build_command_envelope(
        CommandSource::from_in_session_cli(
            session_id,
            Some(client_id),
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
        !delivery_receiver
            .try_iter()
            .any(|delivery| matches!(delivery, Delivery::SwitchTo(_))),
        "a refused switch queues no move"
    );
}

#[test]
fn a_switch_naming_a_client_that_is_not_attached_is_refused() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = runtime
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
            Some(client_id),
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
    let client_id = runtime
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
    let _delivery_receiver = runtime.subscribe(client_id);

    let command_envelope = build_command_envelope(
        CommandSource::from_in_session_cli(
            session_id,
            Some(client_id),
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

    runtime.handle_client_detach(client_id);

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
    let delivery_receiver = runtime.subscribe(initial_client_id);
    let target_session_id = SessionId::new();

    // An external source names no client of its own: `client_id` names the
    // client that moves.
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
        delivery_receiver
            .try_iter()
            .filter_map(|delivery| match delivery {
                Delivery::SwitchTo(session_id) => Some(session_id),
                _ => None,
            })
            .collect::<Vec<SessionId>>(),
        vec![target_session_id]
    );
}

/// If the client's delivery queue is full, the switch is refused.
#[test]
fn a_switch_is_refused_when_the_clients_queue_is_full() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = runtime
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
    let _delivery_receiver = runtime.subscribe(client_id);

    // Nothing reads the queue: publishing its whole capacity fills it.
    let queued_events: Vec<Event> = (0..crate::runtime::bus::SUBSCRIBER_QUEUE_CAPACITY)
        .map(|_| Event::TabCreated(koshi_core::event::TabCreated { tab_id }))
        .collect();
    runtime.publish_events(&queued_events);

    let command_envelope = build_command_envelope(
        CommandSource::from_in_session_cli(
            session_id,
            Some(client_id),
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
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);

    let joining_client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        joining_client_id,
        viewport_size,
        None,
        tab_id,
        None,
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
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);

    let joining_client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        joining_client_id,
        viewport_size,
        None,
        tab_id,
        None,
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
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);

    let client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        client_id,
        viewport_size,
        None,
        tab_id,
        None,
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
        viewport_size,
        None,
        tab_id,
        None,
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
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap the genesis client");
    let (session_id, tab_id, _pane_id) = get_only_session_slot(&runtime);

    let client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        client_id,
        viewport_size,
        None,
        tab_id,
        None,
        SystemTime::now(),
        true,
    );

    runtime.handle_client_attach(
        session_id,
        client_id,
        viewport_size,
        None,
        tab_id,
        None,
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
            reason: RejectReason::MinimumSize,
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
            reason: RejectReason::MinimumSize,
            help: Some("not enough space for a new pane".to_string()),
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .count_pane_records(),
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
    assert_eq!(
        get_command_outcome(&runtime.dispatch(command_envelope)),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ])
    );

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
    let viewport_size = Size {
        column_count: 120,
        row_count: 40,
    };
    let client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap");
    let (session_id, tab_id, first_pane_id) = get_only_session_slot(&runtime);
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let second_pane_id = find_other_pane_id(&runtime, session_id, &[first_pane_id]);

    let reported_pane_size = Size {
        column_count: 60,
        row_count: 20,
    };
    let emitted_events = runtime.handle_client_resize(
        client_id,
        viewport_size,
        Some(PaneArea::Reported(reported_pane_size)),
        None,
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
        emitted_events,
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

/// A reported pane area of `0x0` gives the tab a tab size of `0x0`:
/// every pane is suppressed, so no PTY is resized and every PTY keeps its
/// size.
#[test]
fn a_resize_reporting_a_zero_pane_area_resizes_no_pty() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 120,
        row_count: 40,
    };
    let client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap");
    let (session_id, _tab_id, first_pane_id) = get_only_session_slot(&runtime);
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let second_pane_id = find_other_pane_id(&runtime, session_id, &[first_pane_id]);
    let pty_sizes_before = runtime.pty_size_by_pane_id.clone();
    let first_pane_size_history = fake_pty_backend
        .list_pane_sizes(first_pane_id)
        .expect("resizes");
    let second_pane_size_history = fake_pty_backend
        .list_pane_sizes(second_pane_id)
        .expect("resizes");

    let emitted_events = runtime.handle_client_resize(
        client_id,
        viewport_size,
        Some(PaneArea::Reported(Size {
            column_count: 0,
            row_count: 0,
        })),
        None,
    );

    assert_eq!(emitted_events, Vec::new());
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
    let snapshot = runtime.build_snapshot(client_id).expect("a frame");
    assert!(
        snapshot
            .session_snapshot
            .active_tab_snapshot
            .is_every_pane_suppressed
    );
}

/// A client that reported starving and then reports a size gets the tab
/// resized to that size, one resize per pane.
#[test]
fn a_client_reporting_a_size_after_starving_resizes_each_pane_again() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 120,
        row_count: 40,
    };
    let client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap");
    let (session_id, tab_id, first_pane_id) = get_only_session_slot(&runtime);
    runtime.dispatch(build_command_envelope(
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let second_pane_id = find_other_pane_id(&runtime, session_id, &[first_pane_id]);
    assert_eq!(
        runtime.handle_client_resize(client_id, viewport_size, Some(PaneArea::Starving), None),
        Vec::new()
    );

    let reported_pane_size = Size {
        column_count: 60,
        row_count: 20,
    };
    let emitted_events = runtime.handle_client_resize(
        client_id,
        viewport_size,
        Some(PaneArea::Reported(reported_pane_size)),
        None,
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
        emitted_events,
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
            command_id: ok_command_id,
            emitted_events,
        } => {
            assert_eq!(ok_command_id, command_id);
            assert_eq!(
                list_event_names(&emitted_events),
                ["PaneClosing", "PaneRemoved", "LayoutChanged", "PaneFocused"]
            );
        }
        unexpected_result => panic!("expected Ok, got {unexpected_result:?}"),
    }
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(tab_id),
        Some(right_pane_id)
    );
    assert_eq!(
        runtime.session_by_id[&session_id].tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(right_pane_id)
    );
}

/// The tab's only viewer reports it has no room to draw, so the tab has no
/// tab size and no PTY moves.
#[test]
fn a_resize_reporting_starving_leaves_the_tab_sizes_unchanged() {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap");
    let (_session_id, _tab_id, pane_id) = get_only_session_slot(&runtime);
    let pane_sizes_before_starving_resize =
        fake_pty_backend.list_pane_sizes(pane_id).expect("resizes");

    let emitted_events =
        runtime.handle_client_resize(client_id, viewport_size, Some(PaneArea::Starving), None);

    assert_eq!(emitted_events, Vec::new());
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
    let viewport_size = Size {
        column_count: 120,
        row_count: 40,
    };
    let client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::now())
        .expect("bootstrap");
    let (session_id, tab_id, pane_id) = get_only_session_slot(&runtime);

    let reported_pane_size = PaneArea::Reported(Size {
        column_count: 60,
        row_count: 20,
    });
    runtime.handle_client_attach(
        session_id,
        client_id,
        viewport_size,
        Some(reported_pane_size),
        tab_id,
        None,
        SystemTime::now(),
        false,
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_reported_pane_area(),
        Some(reported_pane_size)
    );

    runtime.handle_client_attach(
        session_id,
        client_id,
        viewport_size,
        None,
        tab_id,
        None,
        SystemTime::now(),
        false,
    );

    let attached_client = runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("client");
    assert_eq!(attached_client.get_reported_pane_area(), None);
    assert_eq!(
        attached_client.get_pane_area(),
        Some(compute_default_pane_area_size(viewport_size))
    );
    assert_eq!(
        *fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("resizes")
            .last()
            .unwrap(),
        compute_root_pane_pty_size(
            pane_id,
            compute_default_pane_area_size(viewport_size),
            PaneSizing::default()
        )
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

// --- Floating pane commands --------------------------------------------------

/// One session seeded for alice, holding one tiled pane and no floating pane.
struct FloatingCommandFixture {
    runtime: Server,
    fake_pty_backend: Arc<FakePtyBackend>,
    session_id: SessionId,
    alice_client_id: ClientId,
    tab_id: TabId,
    tiled_pane_id: PaneId,
}

/// [`bootstrap_alice_session`] with an `80x24` viewport: pane area `80x22`. A
/// floating pane asking for 60% by 60% solves to `48x13`, and a `40x12`
/// floating pane centers at `(20, 5)`.
fn build_floating_command_fixture() -> FloatingCommandFixture {
    bootstrap_alice_session(Size {
        column_count: 80,
        row_count: 24,
    })
}

/// A session seeded by [`Server::bootstrap_local`] for alice, attached at
/// `UNIX_EPOCH` with a `viewport_size` viewport, holding one tiled pane.
fn bootstrap_alice_session(viewport_size: Size) -> FloatingCommandFixture {
    let (mut runtime, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let alice_client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::UNIX_EPOCH)
        .expect("bootstrap alice");
    let (session_id, tab_id, tiled_pane_id) = get_only_session_slot(&runtime);
    FloatingCommandFixture {
        runtime,
        fake_pty_backend,
        session_id,
        alice_client_id,
        tab_id,
        tiled_pane_id,
    }
}

/// A floating `new-pane` request with `size`, `at` and `is_pinned` as given,
/// running the default shell with no working directory and naming no client.
fn build_floating_new_pane_args(
    size: Option<FloatingPaneSize>,
    at: Option<Point>,
    is_pinned: bool,
) -> NewPaneArgs {
    NewPaneArgs {
        placement: NewPanePlacement::Floating {
            size,
            at,
            is_pinned,
        },
        working_directory: None,
        spawn_spec: None,
        client_id: None,
    }
}

/// A floating pane size of `column_count` by `row_count` cells.
fn build_cells_floating_pane_size(column_count: u16, row_count: u16) -> FloatingPaneSize {
    FloatingPaneSize {
        width: FloatingPaneDimension::Cells(
            NonZeroU16::new(column_count).expect("a nonzero column count"),
        ),
        height: FloatingPaneDimension::Cells(
            NonZeroU16::new(row_count).expect("a nonzero row count"),
        ),
    }
}

/// Dispatch `new_pane_args` from `command_source` and return the floating pane
/// it created. Panics unless the command applied and its first event is a
/// [`PaneCreated`] with no tab.
fn create_floating_pane(
    runtime: &mut Server,
    command_source: CommandSource,
    new_pane_args: NewPaneArgs,
) -> PaneId {
    let command_result = runtime.dispatch(build_command_envelope(
        command_source,
        Command::NewPane(new_pane_args),
    ));
    let CommandResult::Ok { emitted_events, .. } = &command_result else {
        panic!("expected the floating pane to be created, got {command_result:?}");
    };
    let Some(Event::PaneCreated(PaneCreated {
        pane_id,
        tab_id: None,
    })) = emitted_events.first()
    else {
        panic!("expected a floating PaneCreated first, got {emitted_events:?}");
    };
    *pane_id
}

/// Attach bob to the fixture's tab with a `viewport_size` viewport and return
/// bob's client id.
fn attach_bob(
    runtime: &mut Server,
    session_id: SessionId,
    tab_id: TabId,
    viewport_size: Size,
) -> ClientId {
    let bob_client_id = ClientId::new();
    runtime.handle_client_attach(
        session_id,
        bob_client_id,
        viewport_size,
        None,
        tab_id,
        None,
        SystemTime::now(),
        false,
    );
    bob_client_id
}

/// The attached client `client_id` of `session_id`.
fn get_attached_client(runtime: &Server, session_id: SessionId, client_id: ClientId) -> &Client {
    runtime.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("the client is attached")
}

/// The member of `session_id`'s floating set that holds `pane_id`.
fn get_floating_member(runtime: &Server, session_id: SessionId, pane_id: PaneId) -> FloatingMember {
    *runtime.session_by_id[&session_id]
        .floating_set
        .list_members()
        .iter()
        .find(|floating_member| floating_member.pane_id == pane_id)
        .expect("the pane floats")
}

#[test]
fn a_floating_new_pane_spawns_unfocused_at_the_default_size() {
    let FloatingCommandFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        alice_client_id,
        tab_id,
        tiled_pane_id,
    } = build_floating_command_fixture();
    let session_revision_before = runtime.session_by_id[&session_id].get_placement_revision();
    let alice_revision =
        get_attached_client(&runtime, session_id, alice_client_id).get_placement_revision();
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(alice_client_id),
        Command::NewPane(build_floating_new_pane_args(None, None, false)),
    );
    let command_id = command_envelope.command_id;

    let command_result = runtime.dispatch(command_envelope);

    let floating_pane_id = find_other_pane_id(&runtime, session_id, &[tiled_pane_id]);
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id,
            emitted_events: vec![
                Event::PaneCreated(PaneCreated {
                    pane_id: floating_pane_id,
                    tab_id: None,
                }),
                build_pty_resized(floating_pane_id, 46, 9),
            ],
        }
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session.floating_set.list_members(),
        [FloatingMember {
            pane_id: floating_pane_id,
            desired_size: DEFAULT_FLOATING_PANE_SIZE,
            solved_size: FloatingPaneSizeSolve::Sized(Size {
                column_count: 48,
                row_count: 13,
            }),
        }]
    );
    assert_eq!(
        session.get_placement_revision(),
        session_revision_before + 1
    );
    assert_eq!(
        *session
            .panes
            .get_pane_record_by_id(floating_pane_id)
            .expect("the floating pane is registered")
            .get_lifecycle(),
        PaneLifecycle::Running
    );
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(tiled_pane_id)
    );
    let alice_client = get_attached_client(&runtime, session_id, alice_client_id);
    assert_eq!(
        alice_client.list_floating_pane_focus_order(),
        Vec::<PaneId>::new()
    );
    assert_eq!(alice_client.get_focused_floating_pane_id(), None);
    assert_eq!(
        alice_client.get_floating_pane_view(floating_pane_id),
        FloatingPaneView::default()
    );
    assert_eq!(
        alice_client.get_focused_pane_id(tab_id),
        Some(tiled_pane_id)
    );
    assert_eq!(alice_client.get_placement_revision(), alice_revision);
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(floating_pane_id)
            .expect("the floating pane spawned"),
        vec![PtySize {
            column_count: 46,
            row_count: 9,
        }]
    );
    assert_eq!(
        runtime.pty_size_by_pane_id[&floating_pane_id],
        PtySize {
            column_count: 46,
            row_count: 9,
        }
    );
    assert!(runtime.live_pane_ids.contains(&floating_pane_id));
}

#[test]
fn a_floating_new_pane_at_a_cell_stores_that_cell_for_the_issuer_alone() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        tab_id,
        ..
    } = build_floating_command_fixture();
    let bob_client_id = attach_bob(
        &mut runtime,
        session_id,
        tab_id,
        Size {
            column_count: 120,
            row_count: 42,
        },
    );
    let alice_revision =
        get_attached_client(&runtime, session_id, alice_client_id).get_placement_revision();
    let bob_revision =
        get_attached_client(&runtime, session_id, bob_client_id).get_placement_revision();

    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, Some(Point { column: 5, row: 2 }), false),
    );

    let alice_client = get_attached_client(&runtime, session_id, alice_client_id);
    assert_eq!(
        alice_client.get_floating_pane_view(floating_pane_id),
        FloatingPaneView {
            position: FloatingPanePosition::Moved(Point { column: 5, row: 2 }),
            is_minimized: false,
        }
    );
    assert_eq!(alice_client.get_placement_revision(), alice_revision + 1);
    let bob_client = get_attached_client(&runtime, session_id, bob_client_id);
    assert_eq!(
        bob_client.get_floating_pane_view(floating_pane_id),
        FloatingPaneView::default()
    );
    assert_eq!(bob_client.get_placement_revision(), bob_revision);
}

#[test]
fn a_pinned_floating_new_pane_without_a_cell_pins_where_the_issuer_draws_it() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        ..
    } = build_floating_command_fixture();

    // 48x13 on 80x22 centers at (16, 4); the second pane is one cascade step
    // further, at (18, 5).
    let first_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, None, true),
    );
    let second_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, None, true),
    );
    let third_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, Some(Point { column: 5, row: 2 }), true),
    );

    let alice_client = get_attached_client(&runtime, session_id, alice_client_id);
    assert_eq!(
        [first_pane_id, second_pane_id, third_pane_id]
            .map(|pane_id| alice_client.get_floating_pane_view(pane_id).position),
        [
            FloatingPanePosition::Pinned(Point { column: 16, row: 4 }),
            FloatingPanePosition::Pinned(Point { column: 18, row: 5 }),
            FloatingPanePosition::Pinned(Point { column: 5, row: 2 }),
        ]
    );
}

#[test]
fn a_thirteenth_floating_pane_is_refused_and_spawns_nothing() {
    let FloatingCommandFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        alice_client_id,
        ..
    } = build_floating_command_fixture();
    for _ in 0..MAX_FLOATING_PANES_PER_SESSION {
        create_floating_pane(
            &mut runtime,
            CommandSource::from_key_binding(alice_client_id),
            build_floating_new_pane_args(None, None, false),
        );
    }
    let floating_members = runtime.session_by_id[&session_id]
        .floating_set
        .list_members()
        .to_vec();
    let spawned_pane_ids = fake_pty_backend.list_spawned_pane_ids();
    let session_revision_before = runtime.session_by_id[&session_id].get_placement_revision();
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(alice_client_id),
        Command::NewPane(build_floating_new_pane_args(None, None, false)),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("a session holds at most 12 floating panes".to_string()),
        }
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.floating_set.list_members(), floating_members);
    assert_eq!(session.panes.count_pane_records(), 13);
    assert_eq!(session.get_placement_revision(), session_revision_before);
    assert_eq!(fake_pty_backend.list_spawned_pane_ids(), spawned_pane_ids);
}

#[test]
fn a_floating_new_pane_whose_child_cannot_launch_commits_nothing() {
    let FloatingCommandFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        alice_client_id,
        tiled_pane_id,
        ..
    } = build_floating_command_fixture();
    fake_pty_backend.fail_spawns_with(PtyError::Spawn {
        detail: "boom".to_string(),
    });
    let session_revision_before = runtime.session_by_id[&session_id].get_placement_revision();
    let alice_revision =
        get_attached_client(&runtime, session_id, alice_client_id).get_placement_revision();
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(alice_client_id),
        Command::NewPane(build_floating_new_pane_args(
            None,
            Some(Point { column: 5, row: 2 }),
            false,
        )),
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
    assert_eq!(session.floating_set.list_members(), []);
    assert_eq!(session.panes.count_pane_records(), 1);
    assert_eq!(session.get_placement_revision(), session_revision_before);
    let alice_client = get_attached_client(&runtime, session_id, alice_client_id);
    assert_eq!(
        serde_json::to_value(alice_client).expect("the client encodes")
            ["floating_pane_view_by_pane_id"],
        serde_json::json!({})
    );
    assert_eq!(alice_client.get_placement_revision(), alice_revision);
    assert_eq!(runtime.live_pane_ids, HashSet::from([tiled_pane_id]));
    assert_eq!(
        fake_pty_backend.list_spawned_pane_ids(),
        vec![tiled_pane_id]
    );
}

#[test]
fn a_floating_new_pane_with_no_client_attached_starts_suppressed_and_cannot_be_placed() {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let tab_id = TabId::new();
    let tiled_pane_id = PaneId::new();
    let mut session = build_bare_session(SessionId::new());
    register_pane_record(&mut session, tiled_pane_id);
    register_session_tab(&mut session, tab_id, tiled_pane_id);
    let session_id = session.session_id;
    runtime.session_by_id.insert(session_id, session);
    let command_source = CommandSource::from_external_cli(Some(session_id), None);

    for (at, is_pinned) in [(Some(Point { column: 5, row: 2 }), false), (None, true)] {
        let command_envelope = build_command_envelope(
            command_source.clone(),
            Command::NewPane(build_floating_new_pane_args(None, at, is_pinned)),
        );
        let command_id = command_envelope.command_id;
        assert_eq!(
            runtime.dispatch(command_envelope),
            CommandResult::Rejected {
                command_id,
                reason: RejectReason::InvalidState,
                help: Some(
                    "no client is attached to place or pin the new floating pane for".to_string()
                ),
            }
        );
    }
    assert_eq!(
        runtime.session_by_id[&session_id]
            .panes
            .count_pane_records(),
        1
    );

    let command_envelope = build_command_envelope(
        command_source,
        Command::NewPane(build_floating_new_pane_args(None, None, false)),
    );
    let command_id = command_envelope.command_id;
    let command_result = runtime.dispatch(command_envelope);

    let floating_pane_id = find_other_pane_id(&runtime, session_id, &[tiled_pane_id]);
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id,
            emitted_events: vec![
                Event::PaneCreated(PaneCreated {
                    pane_id: floating_pane_id,
                    tab_id: None,
                }),
                build_pty_resized(floating_pane_id, 2, 1),
            ],
        }
    );
    assert_eq!(
        get_floating_member(&runtime, session_id, floating_pane_id).solved_size,
        FloatingPaneSizeSolve::Suppressed
    );
}

#[test]
fn a_floating_new_pane_from_an_external_cli_with_two_clients_and_none_named_is_ambiguous() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        tab_id,
        ..
    } = build_floating_command_fixture();
    attach_bob(
        &mut runtime,
        session_id,
        tab_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
    );
    let command_envelope = build_command_envelope(
        CommandSource::from_external_cli(Some(session_id), None),
        Command::NewPane(build_floating_new_pane_args(None, None, false)),
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
        runtime.session_by_id[&session_id]
            .floating_set
            .list_members(),
        []
    );
}

#[test]
fn writing_to_a_floating_pane_delivers_the_bytes_to_its_child() {
    let FloatingCommandFixture {
        mut runtime,
        fake_pty_backend,
        alice_client_id,
        ..
    } = build_floating_command_fixture();
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(alice_client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(floating_pane_id),
            pane_input_bytes: b"ls\r".to_vec(),
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
        fake_pty_backend
            .list_pane_write_bytes(floating_pane_id)
            .expect("the floating pane's child was spawned"),
        vec![b"ls\r".to_vec()]
    );
}

#[test]
fn closing_a_floating_pane_removes_it_from_every_client_and_kills_its_child() {
    let FloatingCommandFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        alice_client_id,
        tab_id,
        tiled_pane_id,
    } = build_floating_command_fixture();
    let bob_client_id = attach_bob(
        &mut runtime,
        session_id,
        tab_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
    );
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, Some(Point { column: 5, row: 2 }), false),
    );
    runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("the seeded session")
        .clients
        .get_client_mut_by_id(bob_client_id)
        .expect("bob is attached")
        .minimize_floating_pane(floating_pane_id);
    let session_revision_before = runtime.session_by_id[&session_id].get_placement_revision();
    let client_revisions = [alice_client_id, bob_client_id].map(|client_id| {
        get_attached_client(&runtime, session_id, client_id).get_placement_revision()
    });
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(alice_client_id),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(floating_pane_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: vec![
                Event::PaneClosing(PaneClosing {
                    pane_id: floating_pane_id,
                }),
                Event::PaneRemoved(PaneRemoved {
                    pane_id: floating_pane_id,
                    tab_id: None,
                }),
            ],
        }
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.floating_set.list_members(), []);
    assert!(session
        .panes
        .get_pane_record_by_id(floating_pane_id)
        .is_none());
    assert_eq!(
        session.get_placement_revision(),
        session_revision_before + 1
    );
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(tiled_pane_id)
    );
    for (client_id, client_revision) in [alice_client_id, bob_client_id]
        .into_iter()
        .zip(client_revisions)
    {
        let client = get_attached_client(&runtime, session_id, client_id);
        assert_eq!(
            client.get_floating_pane_view(floating_pane_id),
            FloatingPaneView::default()
        );
        assert_eq!(client.get_placement_revision(), client_revision + 1);
    }
    assert_eq!(runtime.live_pane_ids, HashSet::from([tiled_pane_id]));
    assert!(!runtime.pty_size_by_pane_id.contains_key(&floating_pane_id));
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, floating_pane_id),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        }]
    );
}

#[test]
fn a_split_or_a_stack_from_a_floating_pane_is_refused() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        tab_id,
        tiled_pane_id,
        ..
    } = build_floating_command_fixture();
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let expected_help = format!("{floating_pane_id} is floating and has no split to divide");
    let in_session_source = CommandSource::from_in_session_cli(
        session_id,
        Some(alice_client_id),
        floating_pane_id,
        PathBuf::from("/run/koshi/session.sock"),
    );
    let refused_requests = [
        (
            CommandSource::from_key_binding(alice_client_id),
            NewPanePlacement::Split {
                source_pane_id: Some(floating_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
        ),
        (
            CommandSource::from_key_binding(alice_client_id),
            NewPanePlacement::Stacked {
                source_pane_id: Some(floating_pane_id),
                tab_id: None,
            },
        ),
        (
            in_session_source,
            NewPanePlacement::Split {
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Down,
            },
        ),
    ];

    for (command_source, placement) in refused_requests {
        let command_envelope = build_command_envelope(
            command_source,
            Command::NewPane(NewPaneArgs {
                placement,
                ..build_new_pane_args()
            }),
        );
        let command_id = command_envelope.command_id;
        assert_eq!(
            runtime.dispatch(command_envelope),
            CommandResult::Rejected {
                command_id,
                reason: RejectReason::InvalidState,
                help: Some(expected_help.clone()),
            }
        );
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(tiled_pane_id)
    );
    assert_eq!(session.panes.count_pane_records(), 2);
}

#[test]
fn moving_or_swapping_a_floating_pane_is_refused() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        tab_id,
        tiled_pane_id,
        ..
    } = build_floating_command_fixture();
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let refused_commands = [
        (
            Command::MovePane(MovePaneArgs {
                pane_id: Some(floating_pane_id),
                direction: Direction::Right,
            }),
            format!("{floating_pane_id} is floating and has no tiled position"),
        ),
        (
            Command::PlacePane(PlacePaneArgs {
                source_pane_id: floating_pane_id,
                placement_target: PanePlacementTarget::Swap {
                    target_pane_id: tiled_pane_id,
                },
                expected_placement_revision: None,
            }),
            format!("{floating_pane_id} is floating; a swap exchanges tiled slots"),
        ),
        (
            Command::PlacePane(PlacePaneArgs {
                source_pane_id: tiled_pane_id,
                placement_target: PanePlacementTarget::Swap {
                    target_pane_id: floating_pane_id,
                },
                expected_placement_revision: None,
            }),
            format!("{floating_pane_id} is floating; a swap exchanges tiled slots"),
        ),
        (
            Command::PlacePane(PlacePaneArgs {
                source_pane_id: floating_pane_id,
                placement_target: PanePlacementTarget::Split {
                    destination_tab_id: tab_id,
                    anchor: PanePlacementAnchor::Tab,
                    direction: Direction::Right,
                },
                expected_placement_revision: None,
            }),
            format!("{floating_pane_id} is floating and holds no tiled slot"),
        ),
    ];

    for (command, expected_help) in refused_commands {
        let command_envelope =
            build_command_envelope(CommandSource::from_key_binding(alice_client_id), command);
        let command_id = command_envelope.command_id;
        assert_eq!(
            runtime.dispatch(command_envelope),
            CommandResult::Rejected {
                command_id,
                reason: RejectReason::InvalidState,
                help: Some(expected_help),
            }
        );
    }
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(tiled_pane_id)
    );
    assert_eq!(
        session
            .floating_set
            .list_members()
            .iter()
            .map(|floating_member| floating_member.pane_id)
            .collect::<Vec<PaneId>>(),
        vec![floating_pane_id]
    );
}

#[test]
fn a_tab_command_or_a_zoom_from_a_floating_panes_shell_is_refused() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        tab_id,
        ..
    } = build_floating_command_fixture();
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let in_session_source = CommandSource::from_in_session_cli(
        session_id,
        Some(alice_client_id),
        floating_pane_id,
        PathBuf::from("/run/koshi/session.sock"),
    );
    let refused_commands = [
        (
            Command::CloseTab(CloseTabArgs::default()),
            format!("{floating_pane_id} is floating and belongs to no tab"),
        ),
        (
            Command::TogglePaneFullscreen,
            format!("{floating_pane_id} is floating; fullscreen fills a tab"),
        ),
    ];

    for (command, expected_help) in refused_commands {
        let command_envelope = build_command_envelope(in_session_source.clone(), command);
        let command_id = command_envelope.command_id;
        assert_eq!(
            runtime.dispatch(command_envelope),
            CommandResult::Rejected {
                command_id,
                reason: RejectReason::InvalidState,
                help: Some(expected_help),
            }
        );
    }
    let session = &runtime.session_by_id[&session_id];
    assert!(session.tabs.contains_key(&tab_id));
    assert_eq!(
        get_attached_client(&runtime, session_id, alice_client_id).get_zoomed_pane_id(tab_id),
        None
    );
}

#[test]
fn scrolling_a_floating_pane_needs_it_shown_on_the_client() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        tab_id,
        ..
    } = build_floating_command_fixture();
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let dispatch_scroll = |runtime: &mut Server| {
        let command_envelope = build_command_envelope(
            CommandSource::from_key_binding(alice_client_id),
            Command::ScrollPane(ScrollPaneArgs {
                pane_id: Some(floating_pane_id),
                scroll_line_count: 3,
            }),
        );
        let command_id = command_envelope.command_id;
        (command_id, runtime.dispatch(command_envelope))
    };

    let (command_id, command_result) = dispatch_scroll(&mut runtime);
    assert_eq!(
        command_result,
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some(format!("{floating_pane_id} is not shown on this client")),
        }
    );

    let alice_client = runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("the seeded session")
        .clients
        .get_client_mut_by_id(alice_client_id)
        .expect("alice is attached");
    assert!(alice_client.focus_floating_pane(floating_pane_id));
    let (command_id, command_result) = dispatch_scroll(&mut runtime);
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );

    runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("the seeded session")
        .clients
        .get_client_mut_by_id(alice_client_id)
        .expect("alice is attached")
        .minimize_floating_pane(floating_pane_id);
    let (command_id, command_result) = dispatch_scroll(&mut runtime);
    assert_eq!(
        command_result,
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some(format!("{floating_pane_id} is minimized")),
        }
    );

    // bob's 3x3 terminal leaves no room for the floating minimum.
    attach_bob(
        &mut runtime,
        session_id,
        tab_id,
        Size {
            column_count: 3,
            row_count: 3,
        },
    );
    let (command_id, command_result) = dispatch_scroll(&mut runtime);
    assert_eq!(
        command_result,
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some(format!(
                "{floating_pane_id} is suppressed; the smallest attached terminal has no room \
                 for it"
            )),
        }
    );
}

/// Dispatch a resize of `pane_id` by `resize_amount_cells` toward `direction`
/// from `command_source`, and return the command id with the result.
fn dispatch_resize_pane(
    runtime: &mut Server,
    command_source: CommandSource,
    pane_id: PaneId,
    direction: Direction,
    resize_amount_cells: i16,
) -> (CommandId, CommandResult) {
    let command_envelope = build_command_envelope(
        command_source,
        Command::ResizePane(ResizePaneArgs {
            pane_id: Some(pane_id),
            direction,
            resize_amount_cells,
        }),
    );
    let command_id = command_envelope.command_id;
    (command_id, runtime.dispatch(command_envelope))
}

#[test]
fn resizing_a_floating_pane_keeps_the_edge_opposite_the_moved_one_where_the_issuer_draws_it() {
    // A 40x12 pane centers at (20, 5) on alice's 80x22 pane area: its left
    // edge is column 20 and its right edge column 60.
    let resize_cases = [
        (
            Direction::Right,
            3,
            build_cells_floating_pane_size(43, 12),
            Point { column: 20, row: 5 },
            PtySize {
                column_count: 41,
                row_count: 8,
            },
        ),
        (
            Direction::Left,
            3,
            build_cells_floating_pane_size(43, 12),
            Point { column: 17, row: 5 },
            PtySize {
                column_count: 41,
                row_count: 8,
            },
        ),
        (
            Direction::Up,
            2,
            build_cells_floating_pane_size(40, 14),
            Point { column: 20, row: 3 },
            PtySize {
                column_count: 38,
                row_count: 10,
            },
        ),
        (
            Direction::Down,
            -2,
            build_cells_floating_pane_size(40, 10),
            Point { column: 20, row: 5 },
            PtySize {
                column_count: 38,
                row_count: 6,
            },
        ),
    ];

    for (
        direction,
        resize_amount_cells,
        expected_desired_size,
        expected_origin,
        expected_pty_size,
    ) in resize_cases
    {
        let FloatingCommandFixture {
            mut runtime,
            session_id,
            alice_client_id,
            tab_id,
            ..
        } = build_floating_command_fixture();
        let bob_client_id = attach_bob(
            &mut runtime,
            session_id,
            tab_id,
            Size {
                column_count: 120,
                row_count: 42,
            },
        );
        let floating_pane_id = create_floating_pane(
            &mut runtime,
            CommandSource::from_key_binding(alice_client_id),
            build_floating_new_pane_args(Some(build_cells_floating_pane_size(40, 12)), None, false),
        );
        let session_revision_before = runtime.session_by_id[&session_id].get_placement_revision();
        let alice_revision =
            get_attached_client(&runtime, session_id, alice_client_id).get_placement_revision();
        let bob_revision =
            get_attached_client(&runtime, session_id, bob_client_id).get_placement_revision();

        let (command_id, command_result) = dispatch_resize_pane(
            &mut runtime,
            CommandSource::from_key_binding(alice_client_id),
            floating_pane_id,
            direction,
            resize_amount_cells,
        );

        assert_eq!(
            command_result,
            CommandResult::Ok {
                command_id,
                emitted_events: vec![Event::PtyResized(PtyResized {
                    pane_id: floating_pane_id,
                    pty_size: expected_pty_size,
                })],
            },
            "{direction:?} {resize_amount_cells}"
        );
        let floating_member = get_floating_member(&runtime, session_id, floating_pane_id);
        assert_eq!(floating_member.desired_size, expected_desired_size);
        assert_eq!(
            runtime.session_by_id[&session_id].get_placement_revision(),
            session_revision_before + 1
        );
        let alice_client = get_attached_client(&runtime, session_id, alice_client_id);
        assert_eq!(
            alice_client.get_floating_pane_view(floating_pane_id),
            FloatingPaneView {
                position: FloatingPanePosition::Moved(expected_origin),
                is_minimized: false,
            },
            "{direction:?} {resize_amount_cells}"
        );
        assert_eq!(alice_client.get_placement_revision(), alice_revision + 1);
        let bob_client = get_attached_client(&runtime, session_id, bob_client_id);
        assert_eq!(
            bob_client.get_floating_pane_view(floating_pane_id),
            FloatingPaneView::default()
        );
        assert_eq!(bob_client.get_placement_revision(), bob_revision);
    }
}

#[test]
fn a_floating_resize_of_zero_cells_is_refused_and_changes_nothing() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        ..
    } = build_floating_command_fixture();
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(Some(build_cells_floating_pane_size(40, 12)), None, false),
    );
    let session_revision_before = runtime.session_by_id[&session_id].get_placement_revision();

    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        floating_pane_id,
        Direction::Right,
        0,
    );

    assert_eq!(
        command_result,
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some("resize size must be non-zero".to_string()),
        }
    );
    assert_eq!(
        get_floating_member(&runtime, session_id, floating_pane_id).desired_size,
        build_cells_floating_pane_size(40, 12)
    );
    assert_eq!(
        runtime.session_by_id[&session_id].get_placement_revision(),
        session_revision_before
    );
}

#[test]
fn a_pinned_floating_pane_refuses_its_left_and_top_edges_and_stays_pinned_on_the_others() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        ..
    } = build_floating_command_fixture();
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(
            Some(build_cells_floating_pane_size(40, 12)),
            Some(Point { column: 20, row: 5 }),
            true,
        ),
    );

    for direction in [Direction::Left, Direction::Up] {
        let (command_id, command_result) = dispatch_resize_pane(
            &mut runtime,
            CommandSource::from_key_binding(alice_client_id),
            floating_pane_id,
            direction,
            3,
        );
        assert_eq!(
            command_result,
            CommandResult::Rejected {
                command_id,
                reason: RejectReason::InvalidState,
                help: Some(format!(
                    "{floating_pane_id} is pinned; its left and top edges do not move"
                )),
            }
        );
    }
    assert_eq!(
        get_floating_member(&runtime, session_id, floating_pane_id).desired_size,
        build_cells_floating_pane_size(40, 12)
    );
    let alice_revision =
        get_attached_client(&runtime, session_id, alice_client_id).get_placement_revision();

    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        floating_pane_id,
        Direction::Right,
        3,
    );

    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id,
            emitted_events: vec![build_pty_resized(floating_pane_id, 41, 8)],
        }
    );
    let alice_client = get_attached_client(&runtime, session_id, alice_client_id);
    assert_eq!(
        alice_client.get_floating_pane_view(floating_pane_id),
        FloatingPaneView {
            position: FloatingPanePosition::Pinned(Point { column: 20, row: 5 }),
            is_minimized: false,
        }
    );
    assert_eq!(alice_client.get_placement_revision(), alice_revision);
}

#[test]
fn growing_a_pinned_floating_pane_stops_at_the_pane_area_edge_and_keeps_its_pinned_cell() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        ..
    } = build_floating_command_fixture();
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(
            Some(build_cells_floating_pane_size(40, 12)),
            Some(Point { column: 20, row: 5 }),
            true,
        ),
    );

    // Column 20 of an 80-column pane area leaves room for 60 columns.
    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        floating_pane_id,
        Direction::Right,
        30,
    );
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id,
            emitted_events: vec![build_pty_resized(floating_pane_id, 58, 8)],
        }
    );
    assert_eq!(
        get_floating_member(&runtime, session_id, floating_pane_id).desired_size,
        build_cells_floating_pane_size(60, 12)
    );

    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        floating_pane_id,
        Direction::Down,
        1,
    );
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id,
            emitted_events: vec![build_pty_resized(floating_pane_id, 58, 9)],
        }
    );
    assert_eq!(
        get_floating_member(&runtime, session_id, floating_pane_id).desired_size,
        build_cells_floating_pane_size(60, 13)
    );
    assert_eq!(
        get_attached_client(&runtime, session_id, alice_client_id)
            .get_floating_pane_view(floating_pane_id),
        FloatingPaneView {
            position: FloatingPanePosition::Pinned(Point { column: 20, row: 5 }),
            is_minimized: false,
        }
    );
}

#[test]
fn resizing_a_pinned_floating_pane_in_a_narrower_pane_area_keeps_its_pinned_cell() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        ..
    } = build_floating_command_fixture();
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(
            Some(build_cells_floating_pane_size(40, 12)),
            Some(Point { column: 20, row: 5 }),
            true,
        ),
    );
    let narrow_pane_area_size = Size {
        column_count: 50,
        row_count: 22,
    };
    let _ = runtime.handle_client_resize(
        alice_client_id,
        Size {
            column_count: 50,
            row_count: 24,
        },
        Some(PaneArea::Reported(narrow_pane_area_size)),
        None,
    );
    let pinned_view = FloatingPaneView {
        position: FloatingPanePosition::Pinned(Point { column: 20, row: 5 }),
        is_minimized: false,
    };
    let alice_revision =
        get_attached_client(&runtime, session_id, alice_client_id).get_placement_revision();

    // The 50-column pane area draws the pane at column 10.
    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        floating_pane_id,
        Direction::Down,
        1,
    );
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id,
            emitted_events: vec![build_pty_resized(floating_pane_id, 38, 9)],
        }
    );
    let alice_client = get_attached_client(&runtime, session_id, alice_client_id);
    assert_eq!(
        alice_client.get_floating_pane_view(floating_pane_id),
        pinned_view
    );
    assert_eq!(alice_client.get_placement_revision(), alice_revision);

    // The 37-column pane is drawn nearer its pinned cell, at column 13.
    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        floating_pane_id,
        Direction::Right,
        -3,
    );
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id,
            emitted_events: vec![build_pty_resized(floating_pane_id, 35, 9)],
        }
    );
    assert_eq!(
        get_attached_client(&runtime, session_id, alice_client_id)
            .get_floating_pane_view(floating_pane_id),
        pinned_view
    );
    assert_eq!(
        place_floating_pane(
            pinned_view.position,
            Size {
                column_count: 37,
                row_count: 13,
            },
            0,
            narrow_pane_area_size,
        )
        .origin,
        Point { column: 13, row: 5 }
    );
}

#[test]
fn a_floating_pane_growth_stops_at_the_pane_area_edge_on_the_moved_side() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        ..
    } = build_floating_command_fixture();
    let left_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(
            Some(build_cells_floating_pane_size(40, 12)),
            Some(Point { column: 2, row: 5 }),
            false,
        ),
    );
    let right_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(
            Some(build_cells_floating_pane_size(40, 12)),
            Some(Point { column: 40, row: 5 }),
            false,
        ),
    );

    // The right edge stays at column 42, so the pane grows by 2 to column 0.
    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        left_pane_id,
        Direction::Left,
        5,
    );
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id,
            emitted_events: vec![build_pty_resized(left_pane_id, 40, 8)],
        }
    );
    assert_eq!(
        get_floating_member(&runtime, session_id, left_pane_id).desired_size,
        build_cells_floating_pane_size(42, 12)
    );
    assert_eq!(
        get_attached_client(&runtime, session_id, alice_client_id)
            .get_floating_pane_view(left_pane_id),
        FloatingPaneView {
            position: FloatingPanePosition::Moved(Point { column: 0, row: 5 }),
            is_minimized: false,
        }
    );

    // Columns 40 to 79 already reach the right edge of the pane area.
    let session_revision_before = runtime.session_by_id[&session_id].get_placement_revision();
    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        right_pane_id,
        Direction::Right,
        3,
    );
    assert_eq!(
        command_result,
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some(format!(
                "{right_pane_id} already reaches the right edge of the pane area"
            )),
        }
    );
    assert_eq!(
        get_floating_member(&runtime, session_id, right_pane_id).desired_size,
        build_cells_floating_pane_size(40, 12)
    );
    assert_eq!(
        runtime.session_by_id[&session_id].get_placement_revision(),
        session_revision_before
    );
}

#[test]
fn a_floating_pane_resize_past_the_minimum_or_the_smallest_terminal_is_refused() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        ..
    } = build_floating_command_fixture();
    // The default pane minimum of 2x1 plus the chrome gives a floating
    // minimum of 4x5.
    let smallest_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(Some(build_cells_floating_pane_size(4, 5)), None, false),
    );
    let widest_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(Some(build_cells_floating_pane_size(80, 12)), None, false),
    );
    let session_revision_before = runtime.session_by_id[&session_id].get_placement_revision();

    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        smallest_pane_id,
        Direction::Left,
        -1,
    );
    assert_eq!(
        command_result,
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::MinimumSize,
            help: Some(format!(
                "{smallest_pane_id} is at its minimum size on that axis"
            )),
        }
    );
    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        widest_pane_id,
        Direction::Right,
        1,
    );
    assert_eq!(
        command_result,
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some(format!(
                "{widest_pane_id} already spans the smallest attached terminal on that axis"
            )),
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id].get_placement_revision(),
        session_revision_before
    );
}

#[test]
fn a_suppressed_floating_pane_refuses_a_resize() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        tab_id,
        ..
    } = build_floating_command_fixture();
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, None, false),
    );
    // bob's 3x3 terminal leaves no room for the floating minimum.
    attach_bob(
        &mut runtime,
        session_id,
        tab_id,
        Size {
            column_count: 3,
            row_count: 3,
        },
    );

    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        floating_pane_id,
        Direction::Right,
        1,
    );

    assert_eq!(
        command_result,
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some(format!(
                "{floating_pane_id} is suppressed; the smallest attached terminal has no room for \
                 it"
            )),
        }
    );
}

#[test]
fn an_external_floating_pane_resize_acts_for_the_named_client() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        tab_id,
        tiled_pane_id,
        ..
    } = build_floating_command_fixture();
    let bob_client_id = attach_bob(
        &mut runtime,
        session_id,
        tab_id,
        Size {
            column_count: 120,
            row_count: 42,
        },
    );
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(Some(build_cells_floating_pane_size(40, 12)), None, false),
    );

    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_external_cli(Some(session_id), None),
        floating_pane_id,
        Direction::Right,
        1,
    );
    assert_eq!(
        command_result,
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetAmbiguous,
            help: Some("several clients are attached; name the target client".to_string()),
        }
    );
    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_external_cli(Some(session_id), Some(ClientId::new())),
        tiled_pane_id,
        Direction::Right,
        1,
    );
    assert_eq!(
        command_result,
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::TargetNotFound,
            help: Some("target client not attached to the session".to_string()),
        }
    );

    // bob's 120x40 pane area centers the 40x12 pane at (40, 14).
    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_external_cli(Some(session_id), Some(bob_client_id)),
        floating_pane_id,
        Direction::Left,
        1,
    );
    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id,
            emitted_events: vec![build_pty_resized(floating_pane_id, 39, 8)],
        }
    );
    assert_eq!(
        get_attached_client(&runtime, session_id, bob_client_id)
            .get_floating_pane_view(floating_pane_id),
        FloatingPaneView {
            position: FloatingPanePosition::Moved(Point {
                column: 39,
                row: 14
            }),
            is_minimized: false,
        }
    );
    assert_eq!(
        get_attached_client(&runtime, session_id, alice_client_id)
            .get_floating_pane_view(floating_pane_id),
        FloatingPaneView::default()
    );
}

#[test]
fn a_floating_pane_whose_child_exits_leaves_at_once_and_a_suppressed_one_stays() {
    let FloatingCommandFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        alice_client_id,
        tab_id,
        tiled_pane_id,
    } = build_floating_command_fixture();
    let exiting_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, Some(Point { column: 5, row: 2 }), false),
    );
    let staying_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, None, false),
    );
    // bob's 3x3 terminal leaves no room: both floating panes are suppressed.
    attach_bob(
        &mut runtime,
        session_id,
        tab_id,
        Size {
            column_count: 3,
            row_count: 3,
        },
    );
    let session_revision_before = runtime.session_by_id[&session_id].get_placement_revision();
    let alice_revision =
        get_attached_client(&runtime, session_id, alice_client_id).get_placement_revision();

    let emitted_events = runtime.handle_child_exit(exiting_pane_id, ExitStatus::ExitCode(0));

    assert_eq!(
        emitted_events,
        vec![
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: exiting_pane_id,
                exit_code: Some(0),
                signal: None,
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: exiting_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: exiting_pane_id,
                tab_id: None,
            }),
        ]
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(
        session.floating_set.list_members(),
        [FloatingMember {
            pane_id: staying_pane_id,
            desired_size: DEFAULT_FLOATING_PANE_SIZE,
            solved_size: FloatingPaneSizeSolve::Suppressed,
        }]
    );
    assert!(session
        .panes
        .get_pane_record_by_id(exiting_pane_id)
        .is_none());
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(staying_pane_id)
            .map(PaneRecord::get_lifecycle),
        Some(&PaneLifecycle::Running)
    );
    assert_eq!(
        session.get_placement_revision(),
        session_revision_before + 1
    );
    let alice_client = get_attached_client(&runtime, session_id, alice_client_id);
    assert_eq!(
        alice_client.get_floating_pane_view(exiting_pane_id),
        FloatingPaneView::default()
    );
    assert_eq!(alice_client.get_placement_revision(), alice_revision + 1);
    assert_eq!(
        runtime.live_pane_ids,
        HashSet::from([tiled_pane_id, staying_pane_id])
    );
    assert!(!runtime.pty_size_by_pane_id.contains_key(&exiting_pane_id));
    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(exiting_pane_id)
            .expect("the pane spawned"),
        vec![KillPolicy::Force]
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(staying_pane_id)
            .expect("the pane spawned"),
        Vec::<KillPolicy>::new()
    );
}

#[test]
fn closing_the_last_tab_ends_every_floating_pane_before_the_quit() {
    let FloatingCommandFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        alice_client_id,
        tab_id,
        tiled_pane_id,
    } = build_floating_command_fixture();
    let first_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let second_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(alice_client_id),
        Command::CloseTab(CloseTabArgs {
            tab_id: Some(tab_id),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: vec![
                Event::PaneClosing(PaneClosing {
                    pane_id: tiled_pane_id,
                }),
                Event::PaneRemoved(PaneRemoved {
                    pane_id: tiled_pane_id,
                    tab_id: Some(tab_id),
                }),
                Event::TabClosed(TabClosed { tab_id }),
                Event::PaneClosing(PaneClosing {
                    pane_id: first_pane_id,
                }),
                Event::PaneRemoved(PaneRemoved {
                    pane_id: first_pane_id,
                    tab_id: None,
                }),
                Event::PaneClosing(PaneClosing {
                    pane_id: second_pane_id,
                }),
                Event::PaneRemoved(PaneRemoved {
                    pane_id: second_pane_id,
                    tab_id: None,
                }),
                Event::Quit(QuitCause::LastTabClosed {
                    tab_id,
                    pane_exit: None,
                }),
            ],
        }
    );
    let session = &runtime.session_by_id[&session_id];
    assert_eq!(session.floating_set.list_members(), []);
    assert_eq!(session.panes.count_pane_records(), 0);
    assert!(!runtime.has_active_panes());
    for pane_id in [tiled_pane_id, first_pane_id, second_pane_id] {
        assert_eq!(
            wait_for_pane_kill_policies(&fake_pty_backend, pane_id),
            vec![KillPolicy::Force]
        );
    }
}

#[test]
fn closing_the_last_tiled_pane_ends_every_floating_pane_under_its_own_close_policy() {
    let FloatingCommandFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        alice_client_id,
        tab_id,
        tiled_pane_id,
    } = build_floating_command_fixture();
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(alice_client_id),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(tiled_pane_id),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Ok {
            command_id,
            emitted_events: vec![
                Event::PaneClosing(PaneClosing {
                    pane_id: tiled_pane_id,
                }),
                Event::PaneRemoved(PaneRemoved {
                    pane_id: tiled_pane_id,
                    tab_id: Some(tab_id),
                }),
                Event::TabClosed(TabClosed { tab_id }),
                Event::PaneClosing(PaneClosing {
                    pane_id: floating_pane_id,
                }),
                Event::PaneRemoved(PaneRemoved {
                    pane_id: floating_pane_id,
                    tab_id: None,
                }),
                Event::Quit(QuitCause::LastTabClosed {
                    tab_id,
                    pane_exit: None,
                }),
            ],
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .floating_set
            .list_members(),
        []
    );
    assert!(!runtime.has_active_panes());
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, tiled_pane_id),
        vec![KillPolicy::Force]
    );
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, floating_pane_id),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        }]
    );
}

#[test]
fn a_pinned_floating_new_pane_the_issuer_does_not_draw_is_refused_and_spawns_nothing() {
    let FloatingCommandFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        alice_client_id,
        tab_id,
        ..
    } = build_floating_command_fixture();
    // A 3x3 terminal for bob leaves no room: the new floating pane is not drawn
    // on bob's screen.
    attach_bob(
        &mut runtime,
        session_id,
        tab_id,
        Size {
            column_count: 3,
            row_count: 3,
        },
    );
    let spawned_pane_ids = fake_pty_backend.list_spawned_pane_ids();
    let command_envelope = build_command_envelope(
        CommandSource::from_key_binding(alice_client_id),
        Command::NewPane(build_floating_new_pane_args(None, None, true)),
    );
    let command_id = command_envelope.command_id;

    assert_eq!(
        runtime.dispatch(command_envelope),
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some(
                "the new floating pane is not drawn on the client's screen, so it has no \
                 position to pin"
                    .to_string()
            ),
        }
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .floating_set
            .list_members(),
        []
    );
    assert_eq!(fake_pty_backend.list_spawned_pane_ids(), spawned_pane_ids);
}

#[test]
fn a_floating_pane_resize_with_no_reported_pane_area_is_refused() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        ..
    } = build_floating_command_fixture();
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(Some(build_cells_floating_pane_size(40, 12)), None, false),
    );
    runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("the fixture's session")
        .clients
        .get_client_mut_by_id(alice_client_id)
        .expect("alice is attached")
        .update_pane_area(Some(PaneArea::Starving));

    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        floating_pane_id,
        Direction::Right,
        1,
    );

    assert_eq!(
        command_result,
        CommandResult::Rejected {
            command_id,
            reason: RejectReason::InvalidState,
            help: Some(
                "no attached terminal has a pane area to size floating panes in".to_string()
            ),
        }
    );
    assert_eq!(
        get_floating_member(&runtime, session_id, floating_pane_id).desired_size,
        build_cells_floating_pane_size(40, 12)
    );
}

#[test]
fn a_floating_pane_resize_for_a_client_with_no_pane_area_changes_no_view() {
    let FloatingCommandFixture {
        mut runtime,
        session_id,
        alice_client_id,
        tab_id,
        ..
    } = build_floating_command_fixture();
    let bob_client_id = attach_bob(
        &mut runtime,
        session_id,
        tab_id,
        Size {
            column_count: 120,
            row_count: 42,
        },
    );
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(Some(build_cells_floating_pane_size(40, 12)), None, false),
    );
    runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("the fixture's session")
        .clients
        .get_client_mut_by_id(bob_client_id)
        .expect("bob is attached")
        .update_pane_area(Some(PaneArea::Starving));
    let session_revision_before = runtime.session_by_id[&session_id].get_placement_revision();
    let alice_revision =
        get_attached_client(&runtime, session_id, alice_client_id).get_placement_revision();
    let bob_revision =
        get_attached_client(&runtime, session_id, bob_client_id).get_placement_revision();

    let (command_id, command_result) = dispatch_resize_pane(
        &mut runtime,
        CommandSource::from_external_cli(Some(session_id), Some(bob_client_id)),
        floating_pane_id,
        Direction::Right,
        3,
    );

    assert_eq!(
        command_result,
        CommandResult::Ok {
            command_id,
            emitted_events: vec![Event::PtyResized(PtyResized {
                pane_id: floating_pane_id,
                pty_size: PtySize {
                    column_count: 41,
                    row_count: 8,
                },
            })],
        }
    );
    assert_eq!(
        get_floating_member(&runtime, session_id, floating_pane_id).desired_size,
        build_cells_floating_pane_size(43, 12)
    );
    assert_eq!(
        runtime.session_by_id[&session_id].get_placement_revision(),
        session_revision_before + 1
    );
    for (client_id, client_revision) in [
        (alice_client_id, alice_revision),
        (bob_client_id, bob_revision),
    ] {
        let attached_client = get_attached_client(&runtime, session_id, client_id);
        assert_eq!(
            attached_client.get_floating_pane_view(floating_pane_id),
            FloatingPaneView::default()
        );
        assert_eq!(attached_client.get_placement_revision(), client_revision);
    }
}

#[test]
fn a_last_tiled_pane_whose_child_exits_ends_every_floating_pane_under_its_own_close_policy() {
    let FloatingCommandFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        alice_client_id,
        tab_id,
        tiled_pane_id,
    } = build_floating_command_fixture();
    let floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let tiled_pane_exit = PaneProcessExited {
        pane_id: tiled_pane_id,
        exit_code: Some(0),
        signal: None,
    };

    let emitted_events = runtime.handle_child_exit(tiled_pane_id, ExitStatus::ExitCode(0));

    assert_eq!(
        emitted_events,
        vec![
            Event::PaneProcessExited(tiled_pane_exit),
            Event::PaneClosing(PaneClosing {
                pane_id: tiled_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: tiled_pane_id,
                tab_id: Some(tab_id),
            }),
            Event::TabClosed(TabClosed { tab_id }),
            Event::PaneClosing(PaneClosing {
                pane_id: floating_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: floating_pane_id,
                tab_id: None,
            }),
            Event::Quit(QuitCause::LastTabClosed {
                tab_id,
                pane_exit: Some(tiled_pane_exit),
            }),
        ]
    );
    assert_eq!(
        runtime.session_by_id[&session_id]
            .floating_set
            .list_members(),
        []
    );
    assert!(!runtime.has_active_panes());
    assert_eq!(
        wait_for_pane_kill_policies(&fake_pty_backend, floating_pane_id),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        }]
    );
}

#[test]
fn a_floating_new_pane_opens_in_the_directory_of_the_pane_it_was_asked_from() {
    let FloatingCommandFixture {
        mut runtime,
        fake_pty_backend,
        session_id,
        alice_client_id,
        tiled_pane_id,
        ..
    } = build_floating_command_fixture();
    fake_pty_backend.set_live_working_directory(tiled_pane_id, "/home/user/project");

    // A key binding opens it where alice's focused pane is.
    let first_floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_key_binding(alice_client_id),
        build_floating_new_pane_args(None, None, false),
    );
    fake_pty_backend.set_live_working_directory(first_floating_pane_id, "/home/user/logs");
    // The shell of a floating pane opens it where that shell is.
    let second_floating_pane_id = create_floating_pane(
        &mut runtime,
        CommandSource::from_in_session_cli(
            session_id,
            Some(alice_client_id),
            first_floating_pane_id,
            PathBuf::from("/run/koshi/session.sock"),
        ),
        build_floating_new_pane_args(None, None, false),
    );

    for (floating_pane_id, expected_working_directory) in [
        (first_floating_pane_id, "/home/user/project"),
        (second_floating_pane_id, "/home/user/logs"),
    ] {
        assert_eq!(
            fake_pty_backend
                .get_spawn_spec(floating_pane_id)
                .expect("the floating pane's child was spawned")
                .working_directory,
            Some(PathBuf::from(expected_working_directory))
        );
    }
}
