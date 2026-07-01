//! End-to-end lifecycle tests driving the session model with a fake PTY backend.
//!
//! Each test spawns its children on a [`FakePtyBackend`], builds the session
//! around the pane ids the backend mints, and then drives behaviour the way a
//! real runtime would: a child-exit fired on the backend is read back off the
//! pane's handle and handed to [`on_child_exit`]; an output chunk pushed to a
//! pane is read back and routed by looking the pane up in the session. Because
//! the backend and the session share one pane id per child, the signal the
//! backend drives lands on the very pane the session is tracking — so these
//! exercise the real wiring between the PTY layer and the session, not the
//! session in isolation.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

use tile_core::event::Event;
use tile_core::geometry::{Point, Rect, Size, SplitDirection};
use tile_core::ids::{ClientId, PaneId, SessionId, TabId};
use tile_core::process::{PtySize, ShellKind, SpawnSpec};
use tile_layout::tree::{LayoutChild, LayoutNode, SplitNode};
use tile_pane::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use tile_pane::pane::policy::PaneExitPolicy;
use tile_pane::pane::state::PaneRecord;
use tile_session::client::{Client, ClientRegistry};
use tile_session::session::cascade::on_child_exit;
use tile_session::session::lifecycle::SessionLifecycle;
use tile_session::session::policy::EmptyTabPolicy;
use tile_session::session::state::{Session, Tab};
use tile_session::session::tab_ops::close_tab;
use tile_test_support::fake_pty::{ExitStatus, FakePtyBackend, PtyBackend, PtyHandle};

/// A fixed epoch timestamp so every lifecycle transition stays deterministic.
const EPOCH: SystemTime = SystemTime::UNIX_EPOCH;

/// The viewport every client and layout solve uses.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A viewport-sized rect for solving a tab's layout.
fn rect() -> Rect {
    Rect::new(Point { x: 0, y: 0 }, VIEWPORT)
}

/// A viewport too small to fit any pane, so focus recovery finds no focusable
/// survivor even when one still exists in the layout.
fn tiny_rect() -> Rect {
    Rect::new(Point { x: 0, y: 0 }, Size { cols: 1, rows: 1 })
}

/// The spawn spec every fake child launches with — the one fact the backend
/// needs; the tests assert on lifecycle, not on the spec.
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
/// The client carries the session's own id, not a fresh one: a client in a
/// session's registry must belong to that session — `Session::validate` rejects
/// the mismatch as `ClientSessionMismatch` — so threading the real id keeps the
/// fixture a state the session would actually accept.
fn focused_client(session_id: SessionId, tab_id: TabId, pane: PaneId) -> Client {
    let mut client = Client::new(ClientId::new(), session_id, EPOCH, VIEWPORT, tab_id);
    client.update_focused_pane(tab_id, pane);
    client
}

/// A session with the given tabs and pane records and no clients yet. Attach
/// clients with [`Session::attach_client`] after building them against
/// `session.id` via [`focused_client`], so client and session never disagree on
/// the session id.
fn session_with(tabs: Vec<Tab>, records: Vec<PaneRecord>) -> Session {
    let mut session = Session::new(SessionId::new(), "main".to_owned(), ClientRegistry::new());
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
                empty_tab_policy,
            )
        }
        None => Vec::new(),
    }
}

/// Read one pending output chunk off a pane's handle and route it by looking the
/// pane up in the session — the runtime's output path. Returns:
/// - `None` when no chunk was pending,
/// - `Some(true)` when a chunk reached a live pane,
/// - `Some(false)` when a chunk arrived but its pane is gone, so it is dropped.
///
/// Distinguishing "no chunk" from "dropped" matters: a post-removal assertion of
/// `Some(false)` proves both that the chunk did arrive at the boundary *and*
/// that the session had no pane to route it to.
fn route_output(session: &Session, handle: &PtyHandle) -> Option<bool> {
    handle
        .try_read_output()
        .map(|_chunk| session.panes.get(handle.pane_id()).is_some())
}

/// The position of the first event matching `pred`, for ordering assertions.
fn position(events: &[Event], pred: impl Fn(&Event) -> bool) -> Option<usize> {
    events.iter().position(pred)
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
    assert!(session.panes.get(b).is_some());
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab_id),
        Some(b)
    );

    // The exit fact threads the code through from the backend, and is emitted
    // before the focus repair it triggers.
    let exited = position(
        &events,
        |e| matches!(e, Event::PaneProcessExited(p) if p.pane_id == a && p.exit_code == Some(0)),
    )
    .expect("the exit is reported with its code");
    let focused = position(
        &events,
        |e| matches!(e, Event::PaneFocused(p) if p.pane_id == b && p.tab_id == tab_id),
    )
    .expect("the survivor is refocused");
    assert!(exited < focused, "the exit is reported before focus repair");
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
        session.clients.get(client_id).unwrap().focused_pane(tab_id),
        Some(b)
    );
    assert!(events.iter().any(
        |e| matches!(e, Event::PaneProcessExited(p) if p.pane_id == a && p.exit_code == Some(0))
    ));
    assert!(!events.iter().any(|e| matches!(e, Event::PaneFocused(_))));
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
        session.clients.get(first_id).unwrap().focused_pane(tab_id),
        Some(b)
    );
    assert_eq!(
        session.clients.get(second_id).unwrap().focused_pane(tab_id),
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
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::TerminalTooSmallEntered(t) if t.client_id == client_id)));
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab_id),
        None
    );
    // The survivor stays — the tab is not empty, just unfocusable for now.
    assert!(session.panes.get(b).is_some());
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
    assert_eq!(session.lifecycle(), &SessionLifecycle::Stopping);

    // The events report the chain in order: the exit, then the tab closing, then
    // the quit it cascades into.
    let exited = position(
        &events,
        |e| matches!(e, Event::PaneProcessExited(p) if p.pane_id == only && p.exit_code == Some(0)),
    )
    .expect("the exit is reported");
    let closed = position(
        &events,
        |e| matches!(e, Event::TabClosed(t) if t.tab_id == tab_id),
    )
    .expect("the tab is closed");
    let quit = position(&events, |e| matches!(e, Event::Quit)).expect("the session quits");
    assert!(
        exited < closed && closed < quit,
        "exit -> tab close -> quit"
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
    assert!(session.tabs.contains_key(&other_tab));
    assert!(session.panes.get(other).is_some());
    assert_eq!(session.lifecycle(), &SessionLifecycle::Starting);
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::TabClosed(t) if t.tab_id == closing_tab)));
    assert!(!events.iter().any(|e| matches!(e, Event::Quit)));
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
    assert!(session.tabs.contains_key(&tab_id));
    assert!(events.iter().any(
        |e| matches!(e, Event::PaneProcessExited(p) if p.pane_id == pane && p.exit_code == Some(1))
    ));
    assert!(!events.iter().any(|e| matches!(e, Event::PaneRemoved(_))));
    assert!(!events.iter().any(|e| matches!(e, Event::Quit)));
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
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::PaneRemoved(p) if p.pane_id == pane)));
    assert!(!events.iter().any(|e| matches!(e, Event::TabClosed(_))));
    assert!(!events.iter().any(|e| matches!(e, Event::Quit)));
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
    assert!(session.tabs.contains_key(&other_tab));
    assert!(session.panes.get(other).is_some());

    // Each pane is reported closing then removed; the tab close lands only after
    // every pane has been torn down.
    for pane in [a, b] {
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::PaneClosing(p) if p.pane_id == pane)));
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::PaneRemoved(p) if p.pane_id == pane)));
    }
    let last_removed = events
        .iter()
        .rposition(|e| matches!(e, Event::PaneRemoved(_)))
        .expect("panes are removed");
    let tab_closed = position(
        &events,
        |e| matches!(e, Event::TabClosed(t) if t.tab_id == multi_tab),
    )
    .expect("the tab is closed");
    assert!(
        last_removed < tab_closed,
        "panes are removed before the tab closes"
    );

    // Closing a tab is a pure state op: it drops the records but never kills the
    // real processes — that is the runtime's job, driven off these events. So the
    // backend recorded no kills against either pane.
    assert!(pty.kills(a).unwrap().is_empty());
    assert!(pty.kills(b).unwrap().is_empty());
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
    let _ = pump_exit(
        &mut session,
        &handle_a,
        tab_id,
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    // The removed pane leaves the tab's focus history; the survivor stays.
    let history = session.tabs[&tab_id].focus_mru();
    assert!(!history.contains(&a));
    assert!(history.contains(&b));
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
    assert_eq!(route_output(&session, &handle_a), Some(true));

    // Remove the pane through a child-exit.
    pty.trigger_child_exit(a, ExitStatus::ExitCode(0))
        .expect("pane a is known to the backend");
    let _ = pump_exit(
        &mut session,
        &handle_a,
        tab_id,
        rect(),
        EmptyTabPolicy::CloseTab,
    );
    assert!(session.panes.get(a).is_none());

    // Output that arrives after removal still reaches the PTY boundary — the
    // backend never knew about the session-side removal — but the session has no
    // pane to route it to, so the chunk arrives (`Some`) and is dropped
    // (`false`).
    pty.push_output(a, b"after".to_vec())
        .expect("the backend still tracks the spawned child");
    assert_eq!(route_output(&session, &handle_a), Some(false));

    // The surviving pane still receives its output — one pane's removal does not
    // poison routing for the rest.
    pty.push_output(b, b"live".to_vec())
        .expect("pane b is known to the backend");
    assert_eq!(route_output(&session, &handle_b), Some(true));
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

    // The fixtures must build a state the session's own validator accepts: a
    // client carrying a foreign session id, or a pane absent from the registry,
    // is a state the session could never reach, and a test built on it would
    // exercise an impossible session and could mask real regressions. This guards
    // the whole fixture set against drifting back into that.
    assert_eq!(session.validate(), Ok(()));
}
