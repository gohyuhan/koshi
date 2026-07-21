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
use koshi_pane::pane::state::PaneRecord;
use koshi_pty::backend::state::PtyBackend;
use koshi_session::client::{Client, ClientRegistry};
use koshi_session::session::state::{Session, Tab};
use koshi_terminal::engine::TerminalEngine;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use crate::runtime::event::RuntimeEvent;
use crate::runtime::render_schedule::FRAME_INTERVAL;
use crate::server::Server;

/// A runtime holding one session, one attached client, and one 1-row terminal
/// engine for a pane — a 1-row screen so each fed newline pushes exactly one
/// line into scrollback. Returns the runtime plus the pane and client ids.
fn runtime_with_pane() -> (Server, PaneId, ClientId) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel::<RuntimeEvent>();
    let mut rt = Server::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx.clone(),
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
fn offset(rt: &Server, client: ClientId, pane: PaneId) -> usize {
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
fn held(rt: &Server, client: ClientId, pane: PaneId) -> bool {
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
/// does on a drag. The highlight sits on row 0 — the oldest line — which the view
/// rules do not care about, until an erase drops every line under it.
fn highlight(rt: &mut Server, client: ClientId, pane: PaneId) {
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
fn clear_highlight(rt: &mut Server, client: ClientId, pane: PaneId) {
    rt.client_mut(client).unwrap().clear_selection(pane);
}

/// The pane engine's current retained scrollback length.
fn retained(rt: &Server, pane: PaneId) -> usize {
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
fn erasing_the_scrollback_leaves_a_live_screen_highlight_held() {
    // ED 3 erases the scrollback and leaves the live screen alone, so a highlight
    // on the live screen still covers exactly the text it did. The offset
    // reclamps to 0 but the highlight keeps holding the view.
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    // Three lines pushed, so the live screen's top row is line 3.
    let selection = Selection {
        kind: SelectionKind::Character,
        anchor: GridPos { row: 3, col: 0 },
        cursor: GridPos { row: 3, col: 4 },
    };
    rt.client_mut(client)
        .unwrap()
        .set_selection(pane, selection);

    rt.handle_pty_output(pane, b"\x1b[3J");
    assert_eq!(retained(&rt, pane), 0);
    assert_eq!(offset(&rt, client, pane), 0);
    assert!(held(&rt, client, pane));
}

#[test]
fn evicting_a_highlights_lines_leaves_the_view_scrolled_up() {
    // A highlight holds the view, so output converts the hold into a rising
    // scroll offset. When the cap then evicts every line under the highlight,
    // the highlight is dropped and the offset simply remains: `offset > 0`
    // with no highlight is exactly the state of a client who scrolled up by
    // hand, and it behaves the same way — the view stays until the client
    // scrolls down.
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    highlight(&mut rt, client, pane); // row 0, in history

    // First storm: the held view's offset rises with the output.
    rt.handle_pty_output(pane, &b"\n".repeat(5_000));
    assert_eq!(offset(&rt, client, pane), 5_000);
    assert!(held(&rt, client, pane));

    // Second storm pushes past the 10 000-line cap: row 0 is evicted.
    rt.handle_pty_output(pane, &b"\n".repeat(6_000));
    assert!(
        rt.client_mut(client).unwrap().selection(pane).is_none(),
        "the highlight's lines are gone, so it is dropped"
    );
    assert_eq!(
        offset(&rt, client, pane),
        10_000,
        "the offset stays, clamped to the oldest retained line"
    );
    assert!(
        held(&rt, client, pane),
        "held by the offset now — an ordinary scrolled-up view"
    );

    // Scrolling down by hand returns to live, like any scrolled-up view.
    rt.scroll_to_bottom(client, pane);
    assert_eq!(offset(&rt, client, pane), 0);
    assert!(!held(&rt, client, pane));
}

#[test]
fn erasing_the_scrollback_drops_a_highlight_that_lived_only_there() {
    // The counterpart: a highlight whose every line the erase removed can never
    // draw again, so it is dropped rather than left holding the view over
    // nothing. The helper's highlight sits on row 0, erased here.
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    highlight(&mut rt, client, pane);

    rt.handle_pty_output(pane, b"\x1b[3J");
    assert_eq!(retained(&rt, pane), 0);
    assert_eq!(offset(&rt, client, pane), 0);
    assert!(!held(&rt, client, pane));
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

/// The engine's effective view offset for the pane — what the renderer actually
/// shows, which is `0` on the alternate screen however far the stored offset sits.
fn effective_offset(rt: &Server, pane: PaneId, stored: usize) -> usize {
    rt.terminal_engines
        .get(&pane)
        .unwrap()
        .state()
        .effective_view_offset(stored)
}

#[test]
fn scrolling_up_on_the_alternate_screen_moves_the_stored_offset_but_shows_live() {
    // The alternate screen keeps no history of its own, but the pane's one
    // scrollback survives entering it (it is restored on exit), so `scroll_up`
    // still clamps against those retained lines and moves the stored offset. The
    // renderer never shows it there, though: the engine's effective offset is 0
    // on the alternate screen.
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n"); // three retained lines
    rt.handle_pty_output(pane, b"\x1b[?1049h"); // enter the alternate screen
    assert_eq!(retained(&rt, pane), 3, "the primary scrollback survives");

    rt.scroll_up(client, pane, 2);
    // The stored offset moved — `scroll_up` clamps to the retained count, which
    // the alternate screen did not clear.
    assert_eq!(offset(&rt, client, pane), 2);
    // But nothing scrolled on screen: the effective offset is 0 on the alt screen.
    assert_eq!(effective_offset(&rt, pane, 2), 0);
}

#[test]
fn scroll_to_top_on_the_alternate_screen_clamps_to_the_retained_history() {
    // `scroll_to_top` is a `scroll_up` by the maximum; on the alternate screen it
    // still lands exactly on the retained primary line count, and still shows
    // nothing scrolled.
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n\n"); // four retained lines
    rt.handle_pty_output(pane, b"\x1b[?1049h");

    rt.scroll_to_top(client, pane);
    assert_eq!(offset(&rt, client, pane), 4); // clamped to the retained count
    assert_eq!(effective_offset(&rt, pane, 4), 0); // still live on screen
}

#[test]
fn a_view_scrolled_while_on_the_alternate_screen_applies_once_it_exits() {
    // Consequence of the two tests above: because the stored offset moves while
    // the alternate screen hides it, leaving the alternate screen re-applies that
    // offset — the primary view is now scrolled back by what was scrolled while
    // the full-screen program was up.
    let (mut rt, pane, client) = runtime_with_pane();
    rt.handle_pty_output(pane, b"\n\n\n");
    rt.handle_pty_output(pane, b"\x1b[?1049h"); // enter
    rt.scroll_up(client, pane, 2);
    assert_eq!(effective_offset(&rt, pane, 2), 0); // hidden while on the alt screen

    rt.handle_pty_output(pane, b"\x1b[?1049l"); // leave the alternate screen
    assert_eq!(retained(&rt, pane), 3, "the primary history is back");
    // The stored offset is unchanged and now shows: the primary view sits two
    // lines back.
    assert_eq!(offset(&rt, client, pane), 2);
    assert_eq!(effective_offset(&rt, pane, 2), 2);
}
