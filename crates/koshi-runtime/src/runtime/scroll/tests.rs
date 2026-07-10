//! Tests for per-client scrollback scrolling: moving a view up into history and
//! back to live, clamping at the ends, and re-anchoring a parked view as new
//! output pushes lines (and reclamping when history shrinks).

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::process::PtySize;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_pane::pane::state::PaneRecord;
use koshi_pty::backend::state::PtyBackend;
use koshi_session::client::{Client, ClientRegistry};
use koshi_session::session::state::{Session, Tab};
use koshi_terminal::engine::TerminalEngine;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use crate::runtime::event::RuntimeEvent;
use crate::runtime::render_schedule::FRAME_INTERVAL;
use crate::runtime::state::Runtime;

/// A runtime holding one session, one attached client, and one 1-row terminal
/// engine for a pane — a 1-row screen so each fed newline pushes exactly one
/// line into scrollback. Returns the runtime plus the pane and client ids.
fn runtime_with_pane() -> (Runtime, PaneId, ClientId) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel::<RuntimeEvent>();
    let mut rt = Runtime::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx.clone(),
        TerminalCleanupGuard::new(),
        Direction::Right,
    );

    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();

    let mut session = Session::new(
        session_id,
        "s".to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session
        .panes
        .insert(PaneRecord::new(pane_id, SystemTime::now()))
        .expect("unique pane id");
    session
        .tabs
        .insert(tab_id, Tab::new(tab_id, "t".to_string(), 0, pane_id));
    let mut client = Client::new(
        client_id,
        session_id,
        SystemTime::now(),
        Size { cols: 8, rows: 1 },
        tab_id,
    );
    client.update_focused_pane(tab_id, pane_id);
    session.attach_client(client);
    rt.sessions.insert(session_id, session);

    rt.terminal_engines
        .insert(pane_id, TerminalEngine::new(PtySize { cols: 8, rows: 1 }));

    (rt, pane_id, client_id)
}

/// The client's current scroll offset for the pane.
fn offset(rt: &Runtime, client: ClientId, pane: PaneId) -> usize {
    rt.sessions()
        .values()
        .next()
        .unwrap()
        .clients
        .get(client)
        .unwrap()
        .scroll_offset(pane)
}

/// The pane engine's current retained scrollback length.
fn retained(rt: &Runtime, pane: PaneId) -> usize {
    rt.terminal_engines
        .get(&pane)
        .unwrap()
        .state()
        .scrollback()
        .len()
}

#[test]
fn scroll_up_moves_into_history_and_clamps_at_the_oldest_line() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n\n\n"); // five lines into scrollback
    assert_eq!(retained(&rt, pane), 5);

    rt.scroll_up(client, pane, 3);
    assert_eq!(offset(&rt, client, pane), 3);

    rt.scroll_up(client, pane, 10); // clamps to the retained count
    assert_eq!(offset(&rt, client, pane), 5);
}

#[test]
fn scroll_down_returns_toward_live_and_stops_following() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n\n\n");
    rt.scroll_up(client, pane, 5);

    rt.scroll_down(client, pane, 2);
    assert_eq!(offset(&rt, client, pane), 3);

    rt.scroll_down(client, pane, 10); // saturates at live
    assert_eq!(offset(&rt, client, pane), 0);
}

#[test]
fn scroll_to_top_and_bottom_jump_to_the_ends() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n\n"); // four lines

    rt.scroll_to_top(client, pane);
    assert_eq!(offset(&rt, client, pane), 4);

    rt.scroll_to_bottom(client, pane);
    assert_eq!(offset(&rt, client, pane), 0);
}

#[test]
fn new_output_anchors_a_scrolled_back_view_to_the_same_history() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n"); // three lines
    rt.scroll_up(client, pane, 2);

    rt.handle_pty_output(pane, b"\n\n"); // two more pushed
                                         // Anchored: the offset rose by the two pushed lines, so the same history
                                         // stays in view instead of drifting.
    assert_eq!(retained(&rt, pane), 5);
    assert_eq!(offset(&rt, client, pane), 4);
}

#[test]
fn new_output_leaves_a_live_following_view_following() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    assert_eq!(offset(&rt, client, pane), 0); // never scrolled

    rt.handle_pty_output(pane, b"\n\n"); // more output
    assert_eq!(offset(&rt, client, pane), 0); // still following live
}

#[test]
fn clearing_scrollback_reclamps_a_parked_view_to_live() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    rt.scroll_up(client, pane, 3);
    assert_eq!(offset(&rt, client, pane), 3);

    rt.handle_pty_output(pane, b"\x1b[3J"); // ED 3: erase scrollback
    assert_eq!(retained(&rt, pane), 0);
    assert_eq!(offset(&rt, client, pane), 0);
}

#[test]
fn a_no_op_scroll_schedules_no_repaint() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n"); // two retained lines
    let now = Instant::now();
    assert!(rt.render_scheduler.poll(now)); // drain the output invalidation

    rt.scroll_down(client, pane, 1); // already following live
    rt.scroll_up(client, pane, 0); // zero-line move
    rt.scroll_up(ClientId::new(), pane, 1); // unknown client
                                            // Nothing marked the frame stale: the loop would sleep until an event.
    assert_eq!(rt.render_scheduler.next_wakeup(now), None);

    rt.scroll_up(client, pane, 1); // a real move marks the frame stale
    assert_eq!(rt.render_scheduler.next_wakeup(now), Some(FRAME_INTERVAL));
}

#[test]
fn anchor_clamps_a_parked_view_to_the_surviving_lines() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    rt.scroll_up(client, pane, 2);

    // Simulate a heavy-truncation frame: five lines pushed but only two survive.
    // The anchor pins the view to the oldest surviving line rather than past it.
    rt.anchor_scrolled_views(pane, 5, 2);
    assert_eq!(offset(&rt, client, pane), 2);
}

#[test]
fn an_erase_and_new_output_in_one_chunk_reanchors_exactly() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n"); // three retained lines
    rt.scroll_up(client, pane, 2);

    // One chunk: ED 3 erases the history, then four lines push fresh history.
    // The monotonic push counter keeps the count exact across the erase, so the
    // parked view rises by all four and clamps to the surviving lines.
    rt.handle_pty_output(pane, b"\x1b[3J\n\n\n\n");
    assert_eq!(retained(&rt, pane), 4);
    assert_eq!(offset(&rt, client, pane), 4);
}

#[test]
fn output_that_touches_no_history_leaves_a_parked_view_alone() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    rt.scroll_up(client, pane, 2);

    rt.handle_pty_output(pane, b"hi"); // prints on the live row, pushes nothing
    assert_eq!(retained(&rt, pane), 3);
    assert_eq!(offset(&rt, client, pane), 2);
}
