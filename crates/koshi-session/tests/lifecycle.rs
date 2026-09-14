//! End-to-end lifecycle tests driving the session model with a fake PTY backend.
//!
//! Each test spawns its children on a [`FakePtyBackend`], builds the session
//! around the pane ids the backend mints, and then drives behaviour the way a
//! real runtime would: a child-exit fired on the backend is read back off the
//! pane's handle and handed to [`on_child_exit`]; an output chunk pushed to a
//! pane is read back and routed by looking the pane up in the session. The
//! backend and the session share one pane id per child, so a signal the backend
//! drives lands on the pane the session tracks.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

use koshi_core::event::{
    Event, LayoutChanged, PaneClosing, PaneFocused, PaneProcessExited, PaneRemoved, TabClosed,
    TerminalTooSmallCause, TerminalTooSmallEntered,
};
use koshi_core::geometry::{Rect, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::process::{PtySize, ShellKind, SpawnSpec};
use koshi_layout::solver::{PaneSizing, MIN_PANE_SIZE};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use koshi_pane::pane::policy::PaneExitPolicy;
use koshi_pane::pane::state::PaneRecord;
use koshi_session::client::{Client, ClientOrigin, ClientRegistry};
use koshi_session::session::cascade::{on_child_exit, remove_pane_cascade};
use koshi_session::session::lifecycle::SessionLifecycle;
use koshi_session::session::policy::EmptyTabPolicy;
use koshi_session::session::state::{Session, Tab};
use koshi_session::session::tab_ops::close_tab;
use koshi_test_support::event_assert::assert_events;
use koshi_test_support::fake_pty::{ExitStatus, FakePtyBackend, PtyBackend, PtyHandle};

/// A fixed epoch timestamp so every lifecycle transition stays deterministic.
const UNIX_EPOCH_TIME: SystemTime = SystemTime::UNIX_EPOCH;

/// The viewport every client and layout solve uses.
const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// A viewport-sized rect for solving a tab's layout.
fn build_viewport_rect() -> Rect {
    Rect::from_size_at_origin(TEST_VIEWPORT_SIZE)
}

/// A viewport too small to fit any pane, so focus recovery finds no focusable
/// survivor even when one still exists in the layout.
fn build_minimum_viewport_rect() -> Rect {
    Rect::from_size_at_origin(Size {
        column_count: 1,
        row_count: 1,
    })
}

/// The spawn spec every fake child launches with: `/bin/zsh`, no arguments, no
/// cwd override, and an empty environment.
fn build_spawn_spec() -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/zsh"),
        arguments: Vec::new(),
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::Zsh,
    }
}

/// The initial PTY size for a spawned child.
fn build_spawn_pty_size() -> PtySize {
    PtySize {
        column_count: 80,
        row_count: 24,
    }
}

/// Spawn a child on the backend, returning the pane id it was spawned under and
/// the live handle that streams its output and exit. The session pane built for
/// this child reuses the same id, so the two refer to one pane.
fn spawn_test_child(pty: &FakePtyBackend) -> (PaneId, PtyHandle) {
    let pane_id = PaneId::new();
    let pane_handle = pty
        .spawn_pane(pane_id, build_spawn_spec(), build_spawn_pty_size())
        .expect("spawn succeeds");
    (pane_handle.get_pane_id(), pane_handle)
}

/// A `Running` terminal pane record sharing the id its fake child was minted
/// with. The fresh `Spawning` record is walked to `Running` through the one
/// legal transition, matching a child whose process has come live.
fn build_running_pane_record(pane_id: PaneId, exit_policy: PaneExitPolicy) -> PaneRecord {
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id, UNIX_EPOCH_TIME);
    pane_record.exit_policy = exit_policy;
    pane_record
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("Spawning -> Running is a legal transition");
    pane_record
}

/// A single-pane tab at display position 0.
fn build_single_pane_tab(tab_id: TabId, pane_id: PaneId) -> Tab {
    Tab::from_root_pane(tab_id, "code".to_owned(), 0, pane_id)
}

/// A single-pane tab at display position `tab_index`.
fn build_tab_at_index(tab_id: TabId, pane_id: PaneId, tab_index: usize) -> Tab {
    let mut tab_state = build_single_pane_tab(tab_id, pane_id);
    tab_state.update_tab_index(tab_index);
    tab_state
}

/// A tab split between `left_pane_id` and `right_pane_id`, at display position 0.
fn build_two_pane_tab(tab_id: TabId, left_pane_id: PaneId, right_pane_id: PaneId) -> Tab {
    let mut tab_state = Tab::from_root_pane(tab_id, "code".to_owned(), 0, left_pane_id);
    tab_state.update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(left_pane_id),
            LayoutNode::Pane(right_pane_id),
        ],
    )));
    tab_state
}

/// A client of `session_id` viewing `tab_id` with `pane_id` focused there.
///
/// The client stores `session_id` as its own, which is what
/// `Session::validate` requires of every client in that session's registry.
fn build_focused_client(session_id: SessionId, tab_id: TabId, pane_id: PaneId) -> Client {
    let mut client = Client::from_attachment(
        ClientId::new(),
        session_id,
        UNIX_EPOCH_TIME,
        TEST_VIEWPORT_SIZE,
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, pane_id);
    client
}

/// A session with the given tabs and pane records and no clients yet. Build
/// clients against `session.session_id` with [`build_focused_client`], then attach them with
/// [`Session::attach_client`].
fn build_session_with(tabs: Vec<Tab>, pane_records: Vec<PaneRecord>) -> Session {
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    for tab in tabs {
        session.tabs.insert(tab.get_tab_id(), tab);
    }
    for pane_record in pane_records {
        session
            .panes
            .register_pane_record(pane_record)
            .expect("unique pane id");
    }
    session
}

/// Read a pane's exit status off its handle and, if its child has exited, drive
/// the session's child-exit cascade — the work a real runtime performs between
/// the PTY backend and the session. Returns the emitted events, or none when the
/// child has not exited (the cascade is edge-driven, not polled).
fn process_child_exit(
    session: &mut Session,
    pane_handle: &PtyHandle,
    tab_id: TabId,
    tab_rect: Rect,
    empty_tab_policy: EmptyTabPolicy,
) -> Vec<Event> {
    match pane_handle.try_receive_exit_status() {
        Some(exit_status) => {
            // A signal-killed child has no exit code, so it maps to `None`.
            let exit_code = match exit_status {
                ExitStatus::ExitCode(code) => Some(code),
                ExitStatus::Signaled(_) => None,
            };
            on_child_exit(
                session,
                tab_id,
                pane_handle.get_pane_id(),
                exit_code,
                tab_rect,
                PaneSizing {
                    minimum_size: MIN_PANE_SIZE,
                    gap_cell_count: 0,
                },
                empty_tab_policy,
            )
        }
        None => Vec::new(),
    }
}

/// Read one pending output chunk off a pane handle and route it by looking the
/// pane up in the session — the runtime's output path. Returns:
/// - `None` when no chunk was pending,
/// - `Some((chunk, true))` when the chunk reached a live pane,
/// - `Some((chunk, false))` when the chunk arrived but its pane is gone, so it is
///   dropped.
fn route_pane_output(session: &Session, pane_handle: &PtyHandle) -> Option<(Vec<u8>, bool)> {
    pane_handle.try_receive_output_chunk().map(|output_chunk| {
        (
            output_chunk,
            session
                .panes
                .get_pane_record_by_id(pane_handle.get_pane_id())
                .is_some(),
        )
    })
}

#[test]
fn child_exit_in_focused_pane_refocuses_a_survivor() {
    let pty = FakePtyBackend::new();
    let (exited_pane_id, exited_pane_handle) = spawn_test_child(&pty);
    let (survivor_pane_id, _survivor_pane_handle) = spawn_test_child(&pty);
    let tab_id = TabId::new();
    let mut session = build_session_with(
        vec![build_two_pane_tab(tab_id, exited_pane_id, survivor_pane_id)],
        vec![
            build_running_pane_record(exited_pane_id, PaneExitPolicy::CloseOnExit),
            build_running_pane_record(survivor_pane_id, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = build_focused_client(session.session_id, tab_id, exited_pane_id);
    let client_id = client.get_client_id();
    session.attach_client(client);

    // No child has exited yet, so a poll is a no-op: the cascade fires on the
    // exit edge, never on an idle poll.
    assert!(process_child_exit(
        &mut session,
        &exited_pane_handle,
        tab_id,
        build_viewport_rect(),
        EmptyTabPolicy::CloseTab
    )
    .is_empty());

    pty.trigger_child_exit(exited_pane_id, ExitStatus::ExitCode(0))
        .expect("the exited pane is known to the backend");
    let events = process_child_exit(
        &mut session,
        &exited_pane_handle,
        tab_id,
        build_viewport_rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The exiting pane is gone; the survivor inherits focus on the client.
    assert!(session
        .panes
        .get_pane_record_by_id(exited_pane_id)
        .is_none());
    assert_eq!(
        *session
            .panes
            .get_pane_record_by_id(survivor_pane_id)
            .expect("the survivor stays")
            .get_lifecycle(),
        PaneLifecycle::Running
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("the client stays attached")
            .get_focused_pane(tab_id),
        Some(survivor_pane_id)
    );

    // The exit fact threads the code through from the backend, and is emitted
    // before the focus repair it triggers.
    assert_events(
        &events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: exited_pane_id,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: exited_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: exited_pane_id,
                tab_id,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id,
                pane_id: survivor_pane_id,
                previous_pane_id: Some(exited_pane_id),
            }),
        ],
    );
}

#[test]
fn a_signal_killed_child_reports_no_exit_code() {
    let pty = FakePtyBackend::new();
    let (exited_pane_id, exited_pane_handle) = spawn_test_child(&pty);
    let (survivor_pane_id, _survivor_pane_handle) = spawn_test_child(&pty);
    let tab_id = TabId::new();
    let mut session = build_session_with(
        vec![build_two_pane_tab(tab_id, exited_pane_id, survivor_pane_id)],
        vec![
            build_running_pane_record(exited_pane_id, PaneExitPolicy::CloseOnExit),
            build_running_pane_record(survivor_pane_id, PaneExitPolicy::CloseOnExit),
        ],
    );

    // A signal-killed child carries no exit code, so the reported code is `None`.
    pty.trigger_child_exit(exited_pane_id, ExitStatus::Signaled(9))
        .expect("the exited pane is known to the backend");
    let events = process_child_exit(
        &mut session,
        &exited_pane_handle,
        tab_id,
        build_viewport_rect(),
        EmptyTabPolicy::CloseTab,
    );

    assert!(session
        .panes
        .get_pane_record_by_id(exited_pane_id)
        .is_none());
    assert_events(
        &events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: exited_pane_id,
                exit_code: None,
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: exited_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: exited_pane_id,
                tab_id,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ],
    );
}

#[test]
fn a_second_exit_for_an_already_removed_pane_only_reports_the_exit() {
    let pty = FakePtyBackend::new();
    let (exited_pane_id, exited_pane_handle) = spawn_test_child(&pty);
    let (survivor_pane_id, _survivor_pane_handle) = spawn_test_child(&pty);
    let tab_id = TabId::new();
    let mut session = build_session_with(
        vec![build_two_pane_tab(tab_id, exited_pane_id, survivor_pane_id)],
        vec![
            build_running_pane_record(exited_pane_id, PaneExitPolicy::CloseOnExit),
            build_running_pane_record(survivor_pane_id, PaneExitPolicy::CloseOnExit),
        ],
    );

    // Two exits queued for one child: the handle hands them back one per read.
    pty.trigger_child_exit(exited_pane_id, ExitStatus::ExitCode(0))
        .expect("the exited pane is known to the backend");
    pty.trigger_child_exit(exited_pane_id, ExitStatus::ExitCode(0))
        .expect("the exited pane is known to the backend");
    let _ = process_child_exit(
        &mut session,
        &exited_pane_handle,
        tab_id,
        build_viewport_rect(),
        EmptyTabPolicy::CloseTab,
    );
    let second_exit_events = process_child_exit(
        &mut session,
        &exited_pane_handle,
        tab_id,
        build_viewport_rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The pane left the registry on the first exit, so the second reports the
    // exit and removes nothing: the survivor and the tab are untouched.
    assert_events(
        &second_exit_events,
        &[Event::PaneProcessExited(PaneProcessExited {
            pane_id: exited_pane_id,
            exit_code: Some(0),
        })],
    );
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree().list_leaf_pane_ids(),
        vec![survivor_pane_id]
    );
}

#[test]
fn child_exit_in_nonfocused_pane_leaves_focus_untouched() {
    let pty = FakePtyBackend::new();
    let (exited_pane_id, exited_pane_handle) = spawn_test_child(&pty);
    let (survivor_pane_id, _survivor_pane_handle) = spawn_test_child(&pty);
    let tab_id = TabId::new();
    let mut session = build_session_with(
        vec![build_two_pane_tab(tab_id, exited_pane_id, survivor_pane_id)],
        vec![
            build_running_pane_record(exited_pane_id, PaneExitPolicy::CloseOnExit),
            build_running_pane_record(survivor_pane_id, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = build_focused_client(session.session_id, tab_id, survivor_pane_id);
    let client_id = client.get_client_id();
    session.attach_client(client);

    pty.trigger_child_exit(exited_pane_id, ExitStatus::ExitCode(0))
        .expect("the exited pane is known to the backend");
    let events = process_child_exit(
        &mut session,
        &exited_pane_handle,
        tab_id,
        build_viewport_rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The exit is still reported, but the focused survivor is untouched and no
    // refocus is emitted.
    assert!(session
        .panes
        .get_pane_record_by_id(exited_pane_id)
        .is_none());
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("the client stays attached")
            .get_focused_pane(tab_id),
        Some(survivor_pane_id)
    );
    assert_events(
        &events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: exited_pane_id,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: exited_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: exited_pane_id,
                tab_id,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ],
    );
}

#[test]
fn child_exit_refocuses_every_client_that_watched_the_pane() {
    let pty = FakePtyBackend::new();
    let (exited_pane_id, exited_pane_handle) = spawn_test_child(&pty);
    let (survivor_pane_id, _survivor_pane_handle) = spawn_test_child(&pty);
    let tab_id = TabId::new();
    let mut session = build_session_with(
        vec![build_two_pane_tab(tab_id, exited_pane_id, survivor_pane_id)],
        vec![
            build_running_pane_record(exited_pane_id, PaneExitPolicy::CloseOnExit),
            build_running_pane_record(survivor_pane_id, PaneExitPolicy::CloseOnExit),
        ],
    );
    let first_client = build_focused_client(session.session_id, tab_id, exited_pane_id);
    let second_client = build_focused_client(session.session_id, tab_id, exited_pane_id);
    let first_client_id = first_client.get_client_id();
    let second_client_id = second_client.get_client_id();
    session.attach_client(first_client);
    session.attach_client(second_client);

    pty.trigger_child_exit(exited_pane_id, ExitStatus::ExitCode(0))
        .expect("the exited pane is known to the backend");
    let _ = process_child_exit(
        &mut session,
        &exited_pane_handle,
        tab_id,
        build_viewport_rect(),
        EmptyTabPolicy::CloseTab,
    );

    // Every client that was looking at the gone pane is moved to the survivor,
    // not just the first one found.
    assert_eq!(
        session
            .clients
            .get_client_by_id(first_client_id)
            .expect("the first client stays attached")
            .get_focused_pane(tab_id),
        Some(survivor_pane_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(second_client_id)
            .expect("the second client stays attached")
            .get_focused_pane(tab_id),
        Some(survivor_pane_id)
    );
}

#[test]
fn child_exit_with_no_room_to_refocus_clears_focus() {
    let pty = FakePtyBackend::new();
    let (exited_pane_id, exited_pane_handle) = spawn_test_child(&pty);
    let (survivor_pane_id, _survivor_pane_handle) = spawn_test_child(&pty);
    let tab_id = TabId::new();
    let mut session = build_session_with(
        vec![build_two_pane_tab(tab_id, exited_pane_id, survivor_pane_id)],
        vec![
            build_running_pane_record(exited_pane_id, PaneExitPolicy::CloseOnExit),
            build_running_pane_record(survivor_pane_id, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = build_focused_client(session.session_id, tab_id, exited_pane_id);
    let client_id = client.get_client_id();
    session.attach_client(client);

    // Drive the exit against a viewport too small to fit the survivor.
    pty.trigger_child_exit(exited_pane_id, ExitStatus::ExitCode(0))
        .expect("the exited pane is known to the backend");
    let events = process_child_exit(
        &mut session,
        &exited_pane_handle,
        tab_id,
        build_minimum_viewport_rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The too-small overlay is reported and the client's stale focus on the gone
    // pane is cleared rather than left dangling on a removed pane.
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("the client stays attached")
            .get_focused_pane(tab_id),
        None
    );
    // The survivor stays — the tab is not empty, only unfocusable at this size.
    assert_eq!(
        *session
            .panes
            .get_pane_record_by_id(survivor_pane_id)
            .expect("the survivor stays")
            .get_lifecycle(),
        PaneLifecycle::Running
    );
    // The overlay names the client's own viewport, its unreported pane area, and
    // the terminal as the cause: the shortage comes from the tab rect, not from
    // this client's regions or another viewer.
    assert_events(
        &events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: exited_pane_id,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: exited_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: exited_pane_id,
                tab_id,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
                client_id,
                viewport_size: TEST_VIEWPORT_SIZE,
                pane_area: None,
                cause: TerminalTooSmallCause::Terminal,
            }),
        ],
    );
}

#[test]
fn last_pane_exit_closes_the_tab_and_quits() {
    let pty = FakePtyBackend::new();
    let (only_pane_id, only_pane_handle) = spawn_test_child(&pty);
    let tab_id = TabId::new();
    let mut session = build_session_with(
        vec![build_single_pane_tab(tab_id, only_pane_id)],
        vec![build_running_pane_record(
            only_pane_id,
            PaneExitPolicy::CloseOnExit,
        )],
    );

    pty.trigger_child_exit(only_pane_id, ExitStatus::ExitCode(0))
        .expect("the pane is known to the backend");
    let events = process_child_exit(
        &mut session,
        &only_pane_handle,
        tab_id,
        build_viewport_rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The empty tab closes and, as the last tab, the session winds down.
    assert!(session.tabs.is_empty());
    assert!(!session.panes.has_pane_records());
    assert_eq!(session.get_lifecycle(), &SessionLifecycle::Stopping);

    // The events report the chain in order: the exit, the pane teardown, the tab
    // closing, then the quit it cascades into. The tab held one pane, so the
    // layout never changes shape and no `LayoutChanged` is emitted.
    assert_events(
        &events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: only_pane_id,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: only_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: only_pane_id,
                tab_id,
            }),
            Event::TabClosed(TabClosed { tab_id }),
            Event::Quit,
        ],
    );
}

#[test]
fn last_pane_exit_in_one_of_several_tabs_does_not_quit() {
    let pty = FakePtyBackend::new();
    let (closing_pane_id, closing_pane_handle) = spawn_test_child(&pty);
    let (other_tab_pane_id, _other_tab_pane_handle) = spawn_test_child(&pty);
    let (closing_tab_id, other_tab_id) = (TabId::new(), TabId::new());
    let mut session = build_session_with(
        vec![
            build_tab_at_index(closing_tab_id, closing_pane_id, 0),
            build_tab_at_index(other_tab_id, other_tab_pane_id, 1),
        ],
        vec![
            build_running_pane_record(closing_pane_id, PaneExitPolicy::CloseOnExit),
            build_running_pane_record(other_tab_pane_id, PaneExitPolicy::CloseOnExit),
        ],
    );

    pty.trigger_child_exit(closing_pane_id, ExitStatus::ExitCode(0))
        .expect("the pane is known to the backend");
    let events = process_child_exit(
        &mut session,
        &closing_pane_handle,
        closing_tab_id,
        build_viewport_rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The emptied tab closes; the sibling tab keeps the session alive and the
    // session lifecycle is not driven toward shutdown.
    assert!(!session.tabs.contains_key(&closing_tab_id));
    assert_eq!(
        *session
            .panes
            .get_pane_record_by_id(other_tab_pane_id)
            .expect("the sibling pane stays")
            .get_lifecycle(),
        PaneLifecycle::Running
    );
    assert_eq!(session.get_lifecycle(), &SessionLifecycle::Starting);
    // The survivor closes ranks: it moves from display position 1 to 0, keeping
    // the tab indexes a dense `0..len`.
    assert_eq!(
        session.tabs[&other_tab_id].get_tab_index(),
        0,
        "the surviving tab takes the closed tab's position"
    );
    assert_events(
        &events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: closing_pane_id,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: closing_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: closing_pane_id,
                tab_id: closing_tab_id,
            }),
            Event::TabClosed(TabClosed {
                tab_id: closing_tab_id,
            }),
        ],
    );
}

#[test]
fn a_failing_last_pane_is_removed_and_the_session_quits() {
    let pty = FakePtyBackend::new();
    let (failed_pane_id, failed_pane_handle) = spawn_test_child(&pty);
    let tab_id = TabId::new();
    let mut session = build_session_with(
        vec![build_single_pane_tab(tab_id, failed_pane_id)],
        vec![build_running_pane_record(
            failed_pane_id,
            PaneExitPolicy::CloseOnExit,
        )],
    );

    pty.trigger_child_exit(failed_pane_id, ExitStatus::ExitCode(1))
        .expect("the pane is known to the backend");
    let events = process_child_exit(
        &mut session,
        &failed_pane_handle,
        tab_id,
        build_viewport_rect(),
        EmptyTabPolicy::CloseTab,
    );

    // A non-zero exit tears down exactly like a clean one: the pane goes, its
    // tab goes with it, and the empty session quits.
    assert_eq!(session.panes.get_pane_record_by_id(failed_pane_id), None);
    assert!(session.tabs.is_empty());
    assert_events(
        &events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: failed_pane_id,
                exit_code: Some(1),
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: failed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: failed_pane_id,
                tab_id,
            }),
            Event::TabClosed(TabClosed { tab_id }),
            Event::Quit,
        ],
    );
}

#[test]
fn closing_the_focused_pane_removes_it_and_refocuses_a_survivor() {
    let pty = FakePtyBackend::new();
    let (closed_pane_id, _closed_pane_handle) = spawn_test_child(&pty);
    let (survivor_pane_id, _survivor_pane_handle) = spawn_test_child(&pty);
    let tab_id = TabId::new();
    let mut session = build_session_with(
        vec![build_two_pane_tab(tab_id, closed_pane_id, survivor_pane_id)],
        vec![
            build_running_pane_record(closed_pane_id, PaneExitPolicy::CloseOnExit),
            build_running_pane_record(survivor_pane_id, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = build_focused_client(session.session_id, tab_id, closed_pane_id);
    let client_id = client.get_client_id();
    session.attach_client(client);

    // An explicit close, not a child exit: the user asks for the focused pane
    // to go while its child is still running.
    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        closed_pane_id,
        build_viewport_rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    // The closed pane is gone, the layout collapsed onto the survivor, and the
    // client that was watching it follows.
    assert!(session
        .panes
        .get_pane_record_by_id(closed_pane_id)
        .is_none());
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(survivor_pane_id)
            .expect("the survivor stays")
            .get_pane_id(),
        survivor_pane_id
    );
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree().list_leaf_pane_ids(),
        vec![survivor_pane_id]
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("the client stays attached")
            .get_focused_pane(tab_id),
        Some(survivor_pane_id)
    );

    // The whole burst, in order. No process-exited event: no child exited.
    assert_events(
        &events,
        &[
            Event::PaneClosing(PaneClosing {
                pane_id: closed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: closed_pane_id,
                tab_id,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id,
                pane_id: survivor_pane_id,
                previous_pane_id: Some(closed_pane_id),
            }),
        ],
    );

    // Closing is a pure state op: the session layer drops the record but never
    // kills the real process, so the backend recorded no kill.
    assert!(pty
        .list_pane_kill_policies(closed_pane_id)
        .expect("the closed pane is known to the backend")
        .is_empty());
}

#[test]
fn closing_a_tab_removes_every_pane_without_killing_via_pty() {
    let pty = FakePtyBackend::new();
    let (first_tab_pane_id, _first_tab_pane_handle) = spawn_test_child(&pty);
    let (second_tab_pane_id, _second_tab_pane_handle) = spawn_test_child(&pty);
    let (other_tab_pane_id, _other_tab_pane_handle) = spawn_test_child(&pty);
    let (multi_tab_id, other_tab_id) = (TabId::new(), TabId::new());
    let mut multi_tab_state =
        build_two_pane_tab(multi_tab_id, first_tab_pane_id, second_tab_pane_id);
    multi_tab_state.update_tab_index(0);
    let mut session = build_session_with(
        vec![
            multi_tab_state,
            build_tab_at_index(other_tab_id, other_tab_pane_id, 1),
        ],
        vec![
            build_running_pane_record(first_tab_pane_id, PaneExitPolicy::CloseOnExit),
            build_running_pane_record(second_tab_pane_id, PaneExitPolicy::CloseOnExit),
            build_running_pane_record(other_tab_pane_id, PaneExitPolicy::CloseOnExit),
        ],
    );

    let events = close_tab(&mut session, multi_tab_id);

    // Every pane the tab held leaves the registry and the tab is gone; the
    // sibling tab and its pane survive.
    assert!(session
        .panes
        .get_pane_record_by_id(first_tab_pane_id)
        .is_none());
    assert!(session
        .panes
        .get_pane_record_by_id(second_tab_pane_id)
        .is_none());
    assert!(!session.tabs.contains_key(&multi_tab_id));
    assert_eq!(
        session.tabs[&other_tab_id]
            .get_layout_tree()
            .list_leaf_pane_ids(),
        vec![other_tab_pane_id]
    );
    assert_eq!(
        *session
            .panes
            .get_pane_record_by_id(other_tab_pane_id)
            .expect("the sibling pane stays")
            .get_lifecycle(),
        PaneLifecycle::Running
    );

    // Each pane is reported closing then removed, in layout order, and the tab
    // close lands only after every pane has been torn down. No client is
    // attached, so nothing is refocused; the sibling tab keeps the session up,
    // so nothing quits.
    assert_events(
        &events,
        &[
            Event::PaneClosing(PaneClosing {
                pane_id: first_tab_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: first_tab_pane_id,
                tab_id: multi_tab_id,
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: second_tab_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: second_tab_pane_id,
                tab_id: multi_tab_id,
            }),
            Event::TabClosed(TabClosed {
                tab_id: multi_tab_id,
            }),
        ],
    );

    // Closing a tab is a pure state op: it drops the records and never kills the
    // real processes, so the backend recorded no kills against either pane.
    assert!(pty
        .list_pane_kill_policies(first_tab_pane_id)
        .expect("the first tab pane is known to the backend")
        .is_empty());
    assert!(pty
        .list_pane_kill_policies(second_tab_pane_id)
        .expect("the second tab pane is known to the backend")
        .is_empty());
}

#[test]
fn child_exit_drops_the_pane_from_focus_history() {
    let pty = FakePtyBackend::new();
    let (exited_pane_id, exited_pane_handle) = spawn_test_child(&pty);
    let (survivor_pane_id, _survivor_pane_handle) = spawn_test_child(&pty);
    let tab_id = TabId::new();
    let mut tab_state = build_two_pane_tab(tab_id, exited_pane_id, survivor_pane_id);
    tab_state.record_focus_mru(survivor_pane_id);
    tab_state.record_focus_mru(exited_pane_id); // newest first: exited, survivor
    let mut session = build_session_with(
        vec![tab_state],
        vec![
            build_running_pane_record(exited_pane_id, PaneExitPolicy::CloseOnExit),
            build_running_pane_record(survivor_pane_id, PaneExitPolicy::CloseOnExit),
        ],
    );

    pty.trigger_child_exit(exited_pane_id, ExitStatus::ExitCode(0))
        .expect("the exited pane is known to the backend");
    let events = process_child_exit(
        &mut session,
        &exited_pane_handle,
        tab_id,
        build_viewport_rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The removed pane leaves the tab's focus history; the survivor stays, at the
    // place it already held.
    assert_eq!(
        session.tabs[&tab_id].list_focus_mru().to_vec(),
        vec![survivor_pane_id]
    );

    // No client watched the pane, so the burst is the exit and the removal
    // alone: the history cleanup is state, never an event of its own.
    assert_events(
        &events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: exited_pane_id,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: exited_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: exited_pane_id,
                tab_id,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ],
    );
}

#[test]
fn output_for_a_removed_pane_is_dropped() {
    let pty = FakePtyBackend::new();
    let (removed_pane_id, removed_pane_handle) = spawn_test_child(&pty);
    let (survivor_pane_id, survivor_pane_handle) = spawn_test_child(&pty);
    let tab_id = TabId::new();
    let mut session = build_session_with(
        vec![build_two_pane_tab(
            tab_id,
            removed_pane_id,
            survivor_pane_id,
        )],
        vec![
            build_running_pane_record(removed_pane_id, PaneExitPolicy::CloseOnExit),
            build_running_pane_record(survivor_pane_id, PaneExitPolicy::CloseOnExit),
        ],
    );

    // While the pane is live, its output routes to a real pane.
    pty.push_output(removed_pane_id, b"before".to_vec())
        .expect("the removed pane is known to the backend");
    assert_eq!(
        route_pane_output(&session, &removed_pane_handle),
        Some((b"before".to_vec(), true))
    );

    // Remove the pane through a child-exit.
    pty.trigger_child_exit(removed_pane_id, ExitStatus::ExitCode(0))
        .expect("the removed pane is known to the backend");
    let events = process_child_exit(
        &mut session,
        &removed_pane_handle,
        tab_id,
        build_viewport_rect(),
        EmptyTabPolicy::CloseTab,
    );
    assert!(session
        .panes
        .get_pane_record_by_id(removed_pane_id)
        .is_none());
    assert_events(
        &events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: removed_pane_id,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: removed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: removed_pane_id,
                tab_id,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ],
    );

    // Output that arrives after removal still reaches the PTY boundary — the
    // backend never knew about the session-side removal — but the session has no
    // pane to route it to, so the chunk arrives (`Some`) and is dropped
    // (`false`).
    pty.push_output(removed_pane_id, b"after".to_vec())
        .expect("the backend still tracks the spawned child");
    assert_eq!(
        route_pane_output(&session, &removed_pane_handle),
        Some((b"after".to_vec(), false))
    );

    // The surviving pane still receives its output — one pane's removal does not
    // poison routing for the rest.
    pty.push_output(survivor_pane_id, b"live".to_vec())
        .expect("the survivor pane is known to the backend");
    assert_eq!(
        route_pane_output(&session, &survivor_pane_handle),
        Some((b"live".to_vec(), true))
    );
}

#[test]
fn a_pane_with_no_pending_output_reads_back_nothing() {
    let pty = FakePtyBackend::new();
    let (pane_id, pane_handle) = spawn_test_child(&pty);
    let tab_id = TabId::new();
    let session = build_session_with(
        vec![build_single_pane_tab(tab_id, pane_id)],
        vec![build_running_pane_record(
            pane_id,
            PaneExitPolicy::CloseOnExit,
        )],
    );

    // A spawn queues no output, so the live pane reads back no chunk at all —
    // the case a dropped chunk (`Some((_, false))`) has to be told apart from.
    assert_eq!(route_pane_output(&session, &pane_handle), None);
}

#[test]
fn the_session_still_validates_after_a_child_exit_refocuses_a_client() {
    let pty = FakePtyBackend::new();
    let (exited_pane_id, exited_pane_handle) = spawn_test_child(&pty);
    let (survivor_pane_id, _survivor_pane_handle) = spawn_test_child(&pty);
    let tab_id = TabId::new();
    let mut session = build_session_with(
        vec![build_two_pane_tab(tab_id, exited_pane_id, survivor_pane_id)],
        vec![
            build_running_pane_record(exited_pane_id, PaneExitPolicy::CloseOnExit),
            build_running_pane_record(survivor_pane_id, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = build_focused_client(session.session_id, tab_id, exited_pane_id);
    session.attach_client(client);

    pty.trigger_child_exit(exited_pane_id, ExitStatus::ExitCode(0))
        .expect("the exited pane is known to the backend");
    let _ = process_child_exit(
        &mut session,
        &exited_pane_handle,
        tab_id,
        build_viewport_rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The cascade leaves no dangling reference behind: the removed pane is out
    // of the layout, out of the registry and out of the client's focus, and the
    // survivor it refocused is a real leaf with a record.
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn fixtures_build_a_consistent_session() {
    let pty = FakePtyBackend::new();
    let (first_pane_id, _first_pane_handle) = spawn_test_child(&pty);
    let (second_pane_id, _second_pane_handle) = spawn_test_child(&pty);
    let tab_id = TabId::new();
    let mut session = build_session_with(
        vec![build_two_pane_tab(tab_id, first_pane_id, second_pane_id)],
        vec![
            build_running_pane_record(first_pane_id, PaneExitPolicy::CloseOnExit),
            build_running_pane_record(second_pane_id, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = build_focused_client(session.session_id, tab_id, first_pane_id);
    session.attach_client(client);

    // The fixtures build a state the session's own validator accepts: every
    // client carries this session's id, and every layout leaf has a registry
    // record.
    assert_eq!(session.validate_session_consistency(), Ok(()));
}
