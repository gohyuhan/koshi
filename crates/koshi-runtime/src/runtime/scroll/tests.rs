//! Tests for per-client scrollback scrolling: moving a view up into history and
//! back to live, clamping at the ends, and re-anchoring a held view as new output
//! pushes lines (and reclamping when history shrinks). A view is held by being
//! scrolled up or by a highlight in that pane, so both reasons are exercised.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use koshi_core::command::{GridPos, Selection, SelectionKind};
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

/// Whether the client's view of the pane is held against live output.
fn held(rt: &Runtime, client: ClientId, pane: PaneId) -> bool {
    rt.sessions()
        .values()
        .next()
        .unwrap()
        .clients
        .get(client)
        .unwrap()
        .is_view_held(pane)
}

/// Put the client in visual mode with a highlight in the pane, as the mouse layer
/// does on a drag. The highlight's shape does not matter to the view rules.
fn highlight(rt: &mut Runtime, client: ClientId, pane: PaneId) {
    let selection = Selection {
        kind: SelectionKind::Character,
        anchor: GridPos { row: 0, col: 0 },
        cursor: GridPos { row: 0, col: 4 },
    };
    rt.client_mut(client)
        .unwrap()
        .set_selection(pane, selection);
}

/// Leave visual mode in `pane`, as a click or any non-copy key landing in it does.
fn clear_highlight(rt: &mut Runtime, client: ClientId, pane: PaneId) {
    rt.client_mut(client).unwrap().clear_selection(pane);
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
    assert!(held(&rt, client, pane)); // scrolled up at all: held

    rt.scroll_up(client, pane, 10); // clamps to the retained count
    assert_eq!(offset(&rt, client, pane), 5);
    assert!(held(&rt, client, pane));
}

#[test]
fn scroll_down_returns_toward_live_and_follows_again_at_the_bottom() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n\n\n");
    rt.scroll_up(client, pane, 5);

    rt.scroll_down(client, pane, 2);
    assert_eq!(offset(&rt, client, pane), 3);
    assert!(held(&rt, client, pane)); // stopped short: still held

    rt.scroll_down(client, pane, 10); // saturates at the newest line
    assert_eq!(offset(&rt, client, pane), 0);
    assert!(!held(&rt, client, pane)); // no highlight: follows live again
}

#[test]
fn scrolling_to_the_bottom_in_visual_mode_keeps_the_view_held() {
    // Scrolling never ends visual mode, so wheeling back to the newest line with
    // a highlight up must not hand the view back to live output — the highlight
    // would slide as soon as the next line printed.
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    highlight(&mut rt, client, pane);
    rt.scroll_up(client, pane, 2);

    rt.scroll_to_bottom(client, pane);
    assert_eq!(offset(&rt, client, pane), 0);
    assert!(held(&rt, client, pane)); // the highlight still holds it

    rt.handle_pty_output(pane, b"\n\n"); // output arrives under the highlight
    assert_eq!(offset(&rt, client, pane), 2); // the view rose with its text
    assert!(held(&rt, client, pane));
}

#[test]
fn scroll_to_top_and_bottom_jump_to_the_ends() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n\n"); // four lines

    rt.scroll_to_top(client, pane);
    assert_eq!(offset(&rt, client, pane), 4);
    assert!(held(&rt, client, pane));

    rt.scroll_to_bottom(client, pane);
    assert_eq!(offset(&rt, client, pane), 0);
    assert!(!held(&rt, client, pane));
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
    assert!(!held(&rt, client, pane));

    rt.handle_pty_output(pane, b"\n\n"); // more output
    assert_eq!(offset(&rt, client, pane), 0); // still following live
    assert!(!held(&rt, client, pane));
}

#[test]
fn new_output_holds_a_highlighted_view_at_the_bottom_on_the_same_lines() {
    // The state an offset alone cannot express, and the reason visual mode holds
    // the view: a highlight at the newest line rises with its text as output
    // pushes, instead of being dragged along by the bottom. Compare the test
    // above — identical offset 0, no highlight, follows.
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    highlight(&mut rt, client, pane);

    rt.handle_pty_output(pane, b"\n\n"); // two lines pushed under it
    assert_eq!(retained(&rt, pane), 5);
    assert_eq!(offset(&rt, client, pane), 2); // rose by the two pushed lines
    assert!(held(&rt, client, pane));
}

#[test]
fn leaving_visual_mode_at_the_bottom_returns_the_view_to_live_output() {
    // The whole point of deriving held: a highlight made at the newest line, then
    // dropped before any output moved the view, leaves nothing holding it — so it
    // follows live again with nothing having to remember to release it.
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    highlight(&mut rt, client, pane);
    assert!(held(&rt, client, pane));

    clear_highlight(&mut rt, client, pane); // a click elsewhere, or any non-copy key
    assert!(!held(&rt, client, pane));

    rt.handle_pty_output(pane, b"\n\n"); // output arrives
    assert_eq!(offset(&rt, client, pane), 0); // followed it down
    assert!(!held(&rt, client, pane));
}

#[test]
fn leaving_visual_mode_leaves_a_view_that_output_pushed_up_held() {
    // Held for the other reason now: while the highlight was up, output pushed the
    // view 2 lines up. Dropping the highlight does not yank the user back to the
    // bottom — being scrolled up holds it, exactly as if they had scrolled there.
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    highlight(&mut rt, client, pane);
    rt.handle_pty_output(pane, b"\n\n");
    assert_eq!(offset(&rt, client, pane), 2);

    clear_highlight(&mut rt, client, pane);
    assert!(held(&rt, client, pane)); // still 2 lines up

    rt.scroll_to_bottom(client, pane); // the user scrolls back down
    assert_eq!(offset(&rt, client, pane), 0);
    assert!(!held(&rt, client, pane)); // and follows live again
}

#[test]
fn output_keeps_arriving_while_a_view_is_held() {
    // Holding holds the view, not the pane: the child's output still reaches the
    // engine and still fills the scrollback underneath.
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    rt.scroll_up(client, pane, 3);

    rt.handle_pty_output(pane, b"\n\n\n\n");
    assert_eq!(retained(&rt, pane), 7); // history kept growing under the held view
    assert_eq!(offset(&rt, client, pane), 7);
    assert!(held(&rt, client, pane));
}

#[test]
fn erasing_the_scrollback_returns_a_scrolled_view_to_live_output() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    rt.scroll_up(client, pane, 3);
    assert_eq!(offset(&rt, client, pane), 3);

    // ED 3 erases the scrollback: the offset reclamps to 0, and with no highlight
    // there is nothing else holding the view, so it follows live again.
    rt.handle_pty_output(pane, b"\x1b[3J");
    assert_eq!(retained(&rt, pane), 0);
    assert_eq!(offset(&rt, client, pane), 0);
    assert!(!held(&rt, client, pane));
}

#[test]
fn erasing_the_scrollback_leaves_a_highlighted_view_held() {
    // ED 3 erases the scrollback and leaves the live screen alone, so a highlight
    // at the newest line still covers exactly the text it did. The offset
    // reclamps to 0 but the highlight keeps holding the view.
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    highlight(&mut rt, client, pane);

    rt.handle_pty_output(pane, b"\x1b[3J");
    assert_eq!(retained(&rt, pane), 0);
    assert_eq!(offset(&rt, client, pane), 0);
    assert!(held(&rt, client, pane));
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
fn anchor_clamps_a_held_view_to_the_surviving_lines() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    rt.scroll_up(client, pane, 2);

    // Simulate a heavy-truncation frame: five lines pushed but only two survive.
    // The anchor holds the view on the oldest surviving line rather than past it.
    rt.anchor_held_views(pane, 5, 2);
    assert_eq!(offset(&rt, client, pane), 2);
    assert!(held(&rt, client, pane));
}

#[test]
fn an_erase_and_new_output_in_one_chunk_reanchors_exactly() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n"); // three retained lines
    rt.scroll_up(client, pane, 2);

    // One chunk: ED 3 erases the history, then four lines push fresh history.
    // The monotonic push counter keeps the count exact across the erase, so the
    // held view rises by all four and clamps to the surviving lines.
    rt.handle_pty_output(pane, b"\x1b[3J\n\n\n\n");
    assert_eq!(retained(&rt, pane), 4);
    assert_eq!(offset(&rt, client, pane), 4);
    assert!(held(&rt, client, pane));
}

#[test]
fn output_that_touches_no_history_leaves_a_held_view_alone() {
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    rt.scroll_up(client, pane, 2);

    rt.handle_pty_output(pane, b"hi"); // prints on the live row, pushes nothing
    assert_eq!(retained(&rt, pane), 3);
    assert_eq!(offset(&rt, client, pane), 2);
    assert!(held(&rt, client, pane));
}

#[test]
fn output_re_anchors_each_client_on_a_shared_pane_on_its_own() {
    // Three clients on one pane — one scrolled up, one holding a highlight at the
    // bottom, one following. The view is per-client, so each is re-anchored alone
    // and none disturbs another.
    let (mut rt, pane, first) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");

    let session_id = *rt.sessions().keys().next().unwrap();
    let tab_id = rt.client_mut(first).unwrap().active_tab();
    let (second, third) = (ClientId::new(), ClientId::new());
    for id in [second, third] {
        let client = Client::new(
            id,
            session_id,
            SystemTime::now(),
            Size { cols: 8, rows: 1 },
            tab_id,
        );
        rt.sessions
            .get_mut(&session_id)
            .unwrap()
            .attach_client(client);
    }

    rt.scroll_up(first, pane, 2);
    highlight(&mut rt, second, pane);
    // `third` is left following live.

    rt.handle_pty_output(pane, b"\n\n"); // two lines pushed

    assert_eq!(offset(&rt, first, pane), 4); // rose by two, still held
    assert!(held(&rt, first, pane));
    assert_eq!(offset(&rt, second, pane), 2); // rose by two from the bottom
    assert!(held(&rt, second, pane));
    assert_eq!(offset(&rt, third, pane), 0); // followed live, untouched
    assert!(!held(&rt, third, pane));
}
