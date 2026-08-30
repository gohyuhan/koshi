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
use koshi_layout::tree::{LayoutChild, LayoutNode, SplitNode};
use koshi_pane::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use koshi_pane::pane::policy::PaneExitPolicy;
use koshi_pane::pane::state::PaneRecord;
use koshi_session::client::{Client, ClientOrigin, ClientRegistry};
use koshi_session::session::cascade::{on_child_exit, remove_pane_cascade};
use koshi_session::session::lifecycle::SessionLifecycle;
use koshi_session::session::policy::EmptyTabPolicy;
use koshi_session::session::state::{Session, Tab};
use koshi_session::session::tab_ops::close_tab;
use koshi_test_support::event_queue::RecordedEvents;
use koshi_test_support::fake_pty::{ExitStatus, FakePtyBackend, PtyBackend, PtyHandle};

/// A fixed epoch timestamp so every lifecycle transition stays deterministic.
const EPOCH: SystemTime = SystemTime::UNIX_EPOCH;

/// The viewport every client and layout solve uses.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A viewport-sized rect for solving a tab's layout.
fn rect() -> Rect {
    Rect::at_origin(VIEWPORT)
}

/// A viewport too small to fit any pane, so focus recovery finds no focusable
/// survivor even when one still exists in the layout.
fn tiny_rect() -> Rect {
    Rect::at_origin(Size { cols: 1, rows: 1 })
}

/// The spawn spec every fake child launches with: `/bin/zsh`, no arguments, no
/// cwd override, and an empty environment.
fn spec() -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/zsh"),
        args: Vec::new(),
        cwd: None,
        env: BTreeMap::new(),
        shell_kind: ShellKind::Zsh,
    }
}

/// The initial PTY size for a spawned child.
fn size() -> PtySize {
    PtySize { cols: 80, rows: 24 }
}

/// Spawn a child on the backend, returning the pane id it was spawned under and
/// the live handle that streams its output and exit. The session pane built for
/// this child reuses the same id, so the two refer to one pane.
fn spawn_child(pty: &FakePtyBackend) -> (PaneId, PtyHandle) {
    let pane_id = PaneId::new();
    let handle = pty.spawn(pane_id, spec(), size()).expect("spawn succeeds");
    (handle.pane_id(), handle)
}

/// A `Running` terminal pane record sharing the id its fake child was minted
/// with. The fresh `Spawning` record is walked to `Running` through the one
/// legal transition, matching a child whose process has come live.
fn running_pane(id: PaneId, exit_policy: PaneExitPolicy) -> PaneRecord {
    let mut record = PaneRecord::new(id, EPOCH);
    record.exit_policy = exit_policy;
    record
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("Spawning -> Running is a legal transition");
    record
}

/// A single-pane tab at display position 0.
fn single_pane_tab(tab_id: TabId, pane: PaneId) -> Tab {
    Tab::new(tab_id, "code".to_owned(), 0, pane)
}

/// A single-pane tab at display position `index`.
fn tab_with_index(tab_id: TabId, pane: PaneId, index: usize) -> Tab {
    let mut tab = single_pane_tab(tab_id, pane);
    tab.update_index(index);
    tab
}

/// A tab split left/right between `left` and `right`, at display position 0.
fn two_pane_tab(tab_id: TabId, left: PaneId, right: PaneId) -> Tab {
    let mut tab = Tab::new(tab_id, "code".to_owned(), 0, left);
    tab.update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutChild::new(LayoutNode::Pane(left)),
            LayoutChild::new(LayoutNode::Pane(right)),
        ],
    )));
    tab
}

/// A client of `session_id` viewing `tab_id` with `pane` focused there.
///
/// The client stores `session_id` as its own, which is what
/// `Session::validate` requires of every client in that session's registry.
fn focused_client(session_id: SessionId, tab_id: TabId, pane: PaneId) -> Client {
    let mut client = Client::new(
        ClientId::new(),
        session_id,
        EPOCH,
        VIEWPORT,
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, pane);
    client
}

/// A session with the given tabs and pane records and no clients yet. Build
/// clients against `session.id` with [`focused_client`], then attach them with
/// [`Session::attach_client`].
fn session_with(tabs: Vec<Tab>, records: Vec<PaneRecord>) -> Session {
    let mut session = Session::new(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    for tab in tabs {
        session.tabs.insert(tab.id(), tab);
    }
    for record in records {
        session.panes.insert(record).expect("unique pane id");
    }
    session
}

/// Read a pane's exit status off its handle and, if its child has exited, drive
/// the session's child-exit cascade — the work a real runtime performs between
/// the PTY backend and the session. Returns the emitted events, or none when the
/// child has not exited (the cascade is edge-driven, not polled).
fn pump_exit(
    session: &mut Session,
    handle: &PtyHandle,
    tab_id: TabId,
    tab_rect: Rect,
    empty_tab_policy: EmptyTabPolicy,
) -> Vec<Event> {
    match handle.try_exit_status() {
        Some(status) => {
            // A signal-killed child has no exit code, so it maps to `None`.
            let exit_code = match status {
                ExitStatus::ExitCode(code) => Some(code),
                ExitStatus::Signaled(_) => None,
            };
            on_child_exit(
                session,
                tab_id,
                handle.pane_id(),
                exit_code,
                EPOCH,
                tab_rect,
                PaneSizing {
                    min: MIN_PANE_SIZE,
                    gap: 0,
                },
                empty_tab_policy,
            )
        }
        None => Vec::new(),
    }
}

/// Read one pending output chunk off a pane's handle and route it by looking the
/// pane up in the session — the runtime's output path. Returns:
/// - `None` when no chunk was pending,
/// - `Some((chunk, true))` when the chunk reached a live pane,
/// - `Some((chunk, false))` when the chunk arrived but its pane is gone, so it is
///   dropped.
fn route_output(session: &Session, handle: &PtyHandle) -> Option<(Vec<u8>, bool)> {
    handle
        .try_read_output()
        .map(|chunk| (chunk, session.panes.get(handle.pane_id()).is_some()))
}

/// Assert an emitted burst is exactly `expected` — same events, same order,
/// nothing extra — through the shared recorder's index-aligned diff.
fn assert_events(events: Vec<Event>, expected: &[Event]) {
    let mut recorded = RecordedEvents::new();
    let mut emitted = events.into_iter();
    recorded.drain_from(|| emitted.next());
    recorded.assert_exact(expected);
}

#[test]
fn child_exit_in_focused_pane_refocuses_a_survivor() {
    let pty = FakePtyBackend::new();
    let (a, handle_a) = spawn_child(&pty);
    let (b, _handle_b) = spawn_child(&pty);
    let tab_id = TabId::new();
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            running_pane(a, PaneExitPolicy::CloseOnExit),
            running_pane(b, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = focused_client(session.id, tab_id, a);
    let client_id = client.id();
    session.attach_client(client);

    // No child has exited yet, so a poll is a no-op: the cascade fires on the
    // exit edge, never on an idle poll.
    assert!(pump_exit(
        &mut session,
        &handle_a,
        tab_id,
        rect(),
        EmptyTabPolicy::CloseTab
    )
    .is_empty());

    pty.trigger_child_exit(a, ExitStatus::ExitCode(0))
        .expect("pane a is known to the backend");
    let events = pump_exit(
        &mut session,
        &handle_a,
        tab_id,
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The exiting pane is gone; the survivor inherits focus on the client.
    assert!(session.panes.get(a).is_none());
    assert_eq!(
        *session
            .panes
            .get(b)
            .expect("the survivor stays")
            .lifecycle(),
        PaneLifecycle::Running
    );
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("the client stays attached")
            .focused_pane(tab_id),
        Some(b)
    );

    // The exit fact threads the code through from the backend, and is emitted
    // before the focus repair it triggers.
    assert_events(
        events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: a,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing { pane_id: a }),
            Event::PaneRemoved(PaneRemoved { pane_id: a, tab_id }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id,
                pane_id: b,
                prior_pane: Some(a),
            }),
        ],
    );
}

#[test]
fn a_signal_killed_child_reports_no_exit_code() {
    let pty = FakePtyBackend::new();
    let (a, handle_a) = spawn_child(&pty);
    let (b, _handle_b) = spawn_child(&pty);
    let tab_id = TabId::new();
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            running_pane(a, PaneExitPolicy::CloseOnExit),
            running_pane(b, PaneExitPolicy::CloseOnExit),
        ],
    );

    // A signal-killed child carries no exit code, so the reported code is `None`.
    pty.trigger_child_exit(a, ExitStatus::Signaled(9))
        .expect("pane a is known to the backend");
    let events = pump_exit(
        &mut session,
        &handle_a,
        tab_id,
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    assert!(session.panes.get(a).is_none());
    assert_events(
        events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: a,
                exit_code: None,
            }),
            Event::PaneClosing(PaneClosing { pane_id: a }),
            Event::PaneRemoved(PaneRemoved { pane_id: a, tab_id }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ],
    );
}

#[test]
fn a_second_exit_for_an_already_removed_pane_only_reports_the_exit() {
    let pty = FakePtyBackend::new();
    let (a, handle_a) = spawn_child(&pty);
    let (b, _handle_b) = spawn_child(&pty);
    let tab_id = TabId::new();
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            running_pane(a, PaneExitPolicy::CloseOnExit),
            running_pane(b, PaneExitPolicy::CloseOnExit),
        ],
    );

    // Two exits queued for one child: the handle hands them back one per read.
    pty.trigger_child_exit(a, ExitStatus::ExitCode(0))
        .expect("pane a is known to the backend");
    pty.trigger_child_exit(a, ExitStatus::ExitCode(0))
        .expect("pane a is known to the backend");
    let _ = pump_exit(
        &mut session,
        &handle_a,
        tab_id,
        rect(),
        EmptyTabPolicy::CloseTab,
    );
    let second = pump_exit(
        &mut session,
        &handle_a,
        tab_id,
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The pane left the registry on the first exit, so the second reports the
    // exit and removes nothing: the survivor and the tab are untouched.
    assert_events(
        second,
        &[Event::PaneProcessExited(PaneProcessExited {
            pane_id: a,
            exit_code: Some(0),
        })],
    );
    assert_eq!(session.tabs[&tab_id].layout().leaf_panes(), vec![b]);
}

#[test]
fn child_exit_in_nonfocused_pane_leaves_focus_untouched() {
    let pty = FakePtyBackend::new();
    let (a, handle_a) = spawn_child(&pty);
    let (b, _handle_b) = spawn_child(&pty);
    let tab_id = TabId::new();
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            running_pane(a, PaneExitPolicy::CloseOnExit),
            running_pane(b, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = focused_client(session.id, tab_id, b); // focused on the survivor
    let client_id = client.id();
    session.attach_client(client);

    pty.trigger_child_exit(a, ExitStatus::ExitCode(0))
        .expect("pane a is known to the backend");
    let events = pump_exit(
        &mut session,
        &handle_a,
        tab_id,
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The exit is still reported, but the focused survivor is untouched and no
    // refocus is emitted.
    assert!(session.panes.get(a).is_none());
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("the client stays attached")
            .focused_pane(tab_id),
        Some(b)
    );
    assert_events(
        events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: a,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing { pane_id: a }),
            Event::PaneRemoved(PaneRemoved { pane_id: a, tab_id }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ],
    );
}

#[test]
fn child_exit_refocuses_every_client_that_watched_the_pane() {
    let pty = FakePtyBackend::new();
    let (a, handle_a) = spawn_child(&pty);
    let (b, _handle_b) = spawn_child(&pty);
    let tab_id = TabId::new();
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            running_pane(a, PaneExitPolicy::CloseOnExit),
            running_pane(b, PaneExitPolicy::CloseOnExit),
        ],
    );
    let first = focused_client(session.id, tab_id, a);
    let second = focused_client(session.id, tab_id, a);
    let (first_id, second_id) = (first.id(), second.id());
    session.attach_client(first);
    session.attach_client(second);

    pty.trigger_child_exit(a, ExitStatus::ExitCode(0))
        .expect("pane a is known to the backend");
    let _ = pump_exit(
        &mut session,
        &handle_a,
        tab_id,
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    // Every client that was looking at the gone pane is moved to the survivor,
    // not just the first one found.
    assert_eq!(
        session
            .clients
            .get(first_id)
            .expect("the first client stays attached")
            .focused_pane(tab_id),
        Some(b)
    );
    assert_eq!(
        session
            .clients
            .get(second_id)
            .expect("the second client stays attached")
            .focused_pane(tab_id),
        Some(b)
    );
}

#[test]
fn child_exit_with_no_room_to_refocus_clears_focus() {
    let pty = FakePtyBackend::new();
    let (a, handle_a) = spawn_child(&pty);
    let (b, _handle_b) = spawn_child(&pty);
    let tab_id = TabId::new();
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            running_pane(a, PaneExitPolicy::CloseOnExit),
            running_pane(b, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = focused_client(session.id, tab_id, a);
    let client_id = client.id();
    session.attach_client(client);

    // Drive the exit against a viewport too small to fit the survivor.
    pty.trigger_child_exit(a, ExitStatus::ExitCode(0))
        .expect("pane a is known to the backend");
    let events = pump_exit(
        &mut session,
        &handle_a,
        tab_id,
        tiny_rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The too-small overlay is reported and the client's stale focus on the gone
    // pane is cleared rather than left dangling on a removed pane.
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("the client stays attached")
            .focused_pane(tab_id),
        None
    );
    // The survivor stays — the tab is not empty, only unfocusable at this size.
    assert_eq!(
        *session
            .panes
            .get(b)
            .expect("the survivor stays")
            .lifecycle(),
        PaneLifecycle::Running
    );
    // The overlay names the client's own viewport, its unreported pane area, and
    // the terminal as the cause: the shortage comes from the tab rect, not from
    // this client's regions or another viewer.
    assert_events(
        events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: a,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing { pane_id: a }),
            Event::PaneRemoved(PaneRemoved { pane_id: a, tab_id }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
                client_id,
                size: VIEWPORT,
                pane_area: None,
                cause: TerminalTooSmallCause::Terminal,
            }),
        ],
    );
}

#[test]
fn last_pane_exit_closes_the_tab_and_quits() {
    let pty = FakePtyBackend::new();
    let (only, handle) = spawn_child(&pty);
    let tab_id = TabId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, only)],
        vec![running_pane(only, PaneExitPolicy::CloseOnExit)],
    );

    pty.trigger_child_exit(only, ExitStatus::ExitCode(0))
        .expect("the pane is known to the backend");
    let events = pump_exit(
        &mut session,
        &handle,
        tab_id,
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The empty tab closes and, as the last tab, the session winds down.
    assert!(session.tabs.is_empty());
    assert!(session.panes.is_empty());
    assert_eq!(session.lifecycle(), &SessionLifecycle::Stopping);

    // The events report the chain in order: the exit, the pane teardown, the tab
    // closing, then the quit it cascades into. The tab held one pane, so the
    // layout never changes shape and no `LayoutChanged` is emitted.
    assert_events(
        events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: only,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing { pane_id: only }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: only,
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
    let (closing, handle) = spawn_child(&pty);
    let (other, _other_handle) = spawn_child(&pty);
    let (closing_tab, other_tab) = (TabId::new(), TabId::new());
    let mut session = session_with(
        vec![
            tab_with_index(closing_tab, closing, 0),
            tab_with_index(other_tab, other, 1),
        ],
        vec![
            running_pane(closing, PaneExitPolicy::CloseOnExit),
            running_pane(other, PaneExitPolicy::CloseOnExit),
        ],
    );

    pty.trigger_child_exit(closing, ExitStatus::ExitCode(0))
        .expect("the pane is known to the backend");
    let events = pump_exit(
        &mut session,
        &handle,
        closing_tab,
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The emptied tab closes; the sibling tab keeps the session alive and the
    // session lifecycle is not driven toward shutdown.
    assert!(!session.tabs.contains_key(&closing_tab));
    assert_eq!(
        *session
            .panes
            .get(other)
            .expect("the sibling pane stays")
            .lifecycle(),
        PaneLifecycle::Running
    );
    assert_eq!(session.lifecycle(), &SessionLifecycle::Starting);
    // The survivor closes ranks: it moves from display position 1 to 0, keeping
    // the tab indexes a dense `0..len`.
    assert_eq!(
        session.tabs[&other_tab].index(),
        0,
        "the surviving tab takes the closed tab's position"
    );
    assert_events(
        events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: closing,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing { pane_id: closing }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: closing,
                tab_id: closing_tab,
            }),
            Event::TabClosed(TabClosed {
                tab_id: closing_tab,
            }),
        ],
    );
}

#[test]
fn last_pane_respawn_policy_keeps_the_pane() {
    let pty = FakePtyBackend::new();
    let (pane, handle) = spawn_child(&pty);
    let tab_id = TabId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, pane)],
        vec![running_pane(pane, PaneExitPolicy::RespawnShell)],
    );

    pty.trigger_child_exit(pane, ExitStatus::ExitCode(1))
        .expect("the pane is known to the backend");
    let events = pump_exit(
        &mut session,
        &handle,
        tab_id,
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    // A respawn-policy pane is not removed: it loops back to `Spawning` for the
    // runtime to relaunch, and nothing tears down.
    let kept = session.panes.get(pane).expect("the pane is kept");
    assert_eq!(*kept.lifecycle(), PaneLifecycle::Spawning);
    assert_eq!(session.tabs[&tab_id].layout().leaf_panes(), vec![pane]);
    // The exit is the whole burst: no removal, no tab close, no quit.
    assert_events(
        events,
        &[Event::PaneProcessExited(PaneProcessExited {
            pane_id: pane,
            exit_code: Some(1),
        })],
    );
}

#[test]
fn last_pane_exit_under_respawn_tab_policy_keeps_the_tab() {
    let pty = FakePtyBackend::new();
    let (pane, handle) = spawn_child(&pty);
    let tab_id = TabId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, pane)],
        vec![running_pane(pane, PaneExitPolicy::CloseOnExit)],
    );

    pty.trigger_child_exit(pane, ExitStatus::ExitCode(0))
        .expect("the pane is known to the backend");
    let events = pump_exit(
        &mut session,
        &handle,
        tab_id,
        rect(),
        EmptyTabPolicy::RespawnShell,
    );

    // The exiting pane is removed, but the empty-tab respawn policy leaves the
    // tab in place for the runtime to refill rather than closing it — so the
    // session does not quit.
    assert!(session.panes.get(pane).is_none());
    assert!(session.tabs.contains_key(&tab_id));
    assert_eq!(session.lifecycle(), &SessionLifecycle::Starting);
    // The burst stops at the removal: the respawn policy emits nothing of its
    // own, so no tab close and no quit follow.
    assert_events(
        events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: pane,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing { pane_id: pane }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pane,
                tab_id,
            }),
        ],
    );
}

#[test]
fn closing_the_focused_pane_removes_it_and_refocuses_a_survivor() {
    let pty = FakePtyBackend::new();
    let (a, _handle_a) = spawn_child(&pty);
    let (b, _handle_b) = spawn_child(&pty);
    let tab_id = TabId::new();
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            running_pane(a, PaneExitPolicy::CloseOnExit),
            running_pane(b, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = focused_client(session.id, tab_id, a);
    let client_id = client.id();
    session.attach_client(client);

    // An explicit close, not a child exit: the user asks for the focused pane
    // to go while its child is still running.
    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        a,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    // The closed pane is gone, the layout collapsed onto the survivor, and the
    // client that was watching it follows.
    assert!(session.panes.get(a).is_none());
    assert_eq!(session.panes.get(b).expect("the survivor stays").id(), b);
    assert_eq!(session.tabs[&tab_id].layout().leaf_panes(), vec![b]);
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("the client stays attached")
            .focused_pane(tab_id),
        Some(b)
    );

    // The whole burst, in order. No process-exited event: no child exited.
    assert_events(
        events,
        &[
            Event::PaneClosing(PaneClosing { pane_id: a }),
            Event::PaneRemoved(PaneRemoved { pane_id: a, tab_id }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id,
                pane_id: b,
                prior_pane: Some(a),
            }),
        ],
    );

    // Closing is a pure state op: the session layer drops the record but never
    // kills the real process, so the backend recorded no kill.
    assert!(pty
        .kills(a)
        .expect("pane a is known to the backend")
        .is_empty());
}

#[test]
fn closing_a_tab_removes_every_pane_without_killing_via_pty() {
    let pty = FakePtyBackend::new();
    let (a, _handle_a) = spawn_child(&pty);
    let (b, _handle_b) = spawn_child(&pty);
    let (other, _other_handle) = spawn_child(&pty);
    let (multi_tab, other_tab) = (TabId::new(), TabId::new());
    let mut multi = two_pane_tab(multi_tab, a, b);
    multi.update_index(0);
    let mut session = session_with(
        vec![multi, tab_with_index(other_tab, other, 1)],
        vec![
            running_pane(a, PaneExitPolicy::CloseOnExit),
            running_pane(b, PaneExitPolicy::CloseOnExit),
            running_pane(other, PaneExitPolicy::CloseOnExit),
        ],
    );

    let events = close_tab(&mut session, multi_tab);

    // Every pane the tab held leaves the registry and the tab is gone; the
    // sibling tab and its pane survive.
    assert!(session.panes.get(a).is_none());
    assert!(session.panes.get(b).is_none());
    assert!(!session.tabs.contains_key(&multi_tab));
    assert_eq!(session.tabs[&other_tab].layout().leaf_panes(), vec![other]);
    assert_eq!(
        *session
            .panes
            .get(other)
            .expect("the sibling pane stays")
            .lifecycle(),
        PaneLifecycle::Running
    );

    // Each pane is reported closing then removed, in layout order, and the tab
    // close lands only after every pane has been torn down. No client is
    // attached, so nothing is refocused; the sibling tab keeps the session up,
    // so nothing quits.
    assert_events(
        events,
        &[
            Event::PaneClosing(PaneClosing { pane_id: a }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: a,
                tab_id: multi_tab,
            }),
            Event::PaneClosing(PaneClosing { pane_id: b }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: b,
                tab_id: multi_tab,
            }),
            Event::TabClosed(TabClosed { tab_id: multi_tab }),
        ],
    );

    // Closing a tab is a pure state op: it drops the records and never kills the
    // real processes, so the backend recorded no kills against either pane.
    assert!(pty
        .kills(a)
        .expect("pane a is known to the backend")
        .is_empty());
    assert!(pty
        .kills(b)
        .expect("pane b is known to the backend")
        .is_empty());
}

#[test]
fn child_exit_drops_the_pane_from_focus_history() {
    let pty = FakePtyBackend::new();
    let (a, handle_a) = spawn_child(&pty);
    let (b, _handle_b) = spawn_child(&pty);
    let tab_id = TabId::new();
    let mut tab = two_pane_tab(tab_id, a, b);
    tab.record_focus_mru(b);
    tab.record_focus_mru(a); // history, newest first: [a, b]
    let mut session = session_with(
        vec![tab],
        vec![
            running_pane(a, PaneExitPolicy::CloseOnExit),
            running_pane(b, PaneExitPolicy::CloseOnExit),
        ],
    );

    pty.trigger_child_exit(a, ExitStatus::ExitCode(0))
        .expect("pane a is known to the backend");
    let events = pump_exit(
        &mut session,
        &handle_a,
        tab_id,
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The removed pane leaves the tab's focus history; the survivor stays, at the
    // place it already held.
    assert_eq!(session.tabs[&tab_id].focus_mru().to_vec(), vec![b]);

    // No client watched the pane, so the burst is the exit and the removal
    // alone: the history cleanup is state, never an event of its own.
    assert_events(
        events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: a,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing { pane_id: a }),
            Event::PaneRemoved(PaneRemoved { pane_id: a, tab_id }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ],
    );
}

#[test]
fn output_for_a_removed_pane_is_dropped() {
    let pty = FakePtyBackend::new();
    let (a, handle_a) = spawn_child(&pty);
    let (b, handle_b) = spawn_child(&pty);
    let tab_id = TabId::new();
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            running_pane(a, PaneExitPolicy::CloseOnExit),
            running_pane(b, PaneExitPolicy::CloseOnExit),
        ],
    );

    // While the pane is live, its output routes to a real pane.
    pty.push_output(a, b"before".to_vec())
        .expect("pane a is known to the backend");
    assert_eq!(
        route_output(&session, &handle_a),
        Some((b"before".to_vec(), true))
    );

    // Remove the pane through a child-exit.
    pty.trigger_child_exit(a, ExitStatus::ExitCode(0))
        .expect("pane a is known to the backend");
    let events = pump_exit(
        &mut session,
        &handle_a,
        tab_id,
        rect(),
        EmptyTabPolicy::CloseTab,
    );
    assert!(session.panes.get(a).is_none());
    assert_events(
        events,
        &[
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: a,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing { pane_id: a }),
            Event::PaneRemoved(PaneRemoved { pane_id: a, tab_id }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ],
    );

    // Output that arrives after removal still reaches the PTY boundary — the
    // backend never knew about the session-side removal — but the session has no
    // pane to route it to, so the chunk arrives (`Some`) and is dropped
    // (`false`).
    pty.push_output(a, b"after".to_vec())
        .expect("the backend still tracks the spawned child");
    assert_eq!(
        route_output(&session, &handle_a),
        Some((b"after".to_vec(), false))
    );

    // The surviving pane still receives its output — one pane's removal does not
    // poison routing for the rest.
    pty.push_output(b, b"live".to_vec())
        .expect("pane b is known to the backend");
    assert_eq!(
        route_output(&session, &handle_b),
        Some((b"live".to_vec(), true))
    );
}

#[test]
fn a_pane_with_no_pending_output_reads_back_nothing() {
    let pty = FakePtyBackend::new();
    let (pane, handle) = spawn_child(&pty);
    let tab_id = TabId::new();
    let session = session_with(
        vec![single_pane_tab(tab_id, pane)],
        vec![running_pane(pane, PaneExitPolicy::CloseOnExit)],
    );

    // A spawn queues no output, so the live pane reads back no chunk at all —
    // the case a dropped chunk (`Some((_, false))`) has to be told apart from.
    assert_eq!(route_output(&session, &handle), None);
}

#[test]
fn the_session_still_validates_after_a_child_exit_refocuses_a_client() {
    let pty = FakePtyBackend::new();
    let (a, handle_a) = spawn_child(&pty);
    let (b, _handle_b) = spawn_child(&pty);
    let tab_id = TabId::new();
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            running_pane(a, PaneExitPolicy::CloseOnExit),
            running_pane(b, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = focused_client(session.id, tab_id, a);
    session.attach_client(client);

    pty.trigger_child_exit(a, ExitStatus::ExitCode(0))
        .expect("pane a is known to the backend");
    let _ = pump_exit(
        &mut session,
        &handle_a,
        tab_id,
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The cascade leaves no dangling reference behind: the removed pane is out
    // of the layout, out of the registry and out of the client's focus, and the
    // survivor it refocused is a real leaf with a record.
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn fixtures_build_a_consistent_session() {
    let pty = FakePtyBackend::new();
    let (a, _handle_a) = spawn_child(&pty);
    let (b, _handle_b) = spawn_child(&pty);
    let tab_id = TabId::new();
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            running_pane(a, PaneExitPolicy::CloseOnExit),
            running_pane(b, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = focused_client(session.id, tab_id, a);
    session.attach_client(client);

    // The fixtures build a state the session's own validator accepts: every
    // client carries this session's id, and every layout leaf has a registry
    // record.
    assert_eq!(session.validate(), Ok(()));
}
