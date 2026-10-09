//! Tests for per-client scrollback scrolling: moving a view up into history and
//! back to live, clamping at the ends, and re-anchoring a held view as new output
//! pushes lines (and reclamping when history shrinks). A view is held by being
//! scrolled up or by a highlight in that pane, so both reasons are exercised.

use super::*;

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use crate::runtime::pty_inbox::InboxSink;
use koshi_core::command::{GridPosition, Selection, SelectionKind};
use koshi_core::geometry::Size;
use koshi_core::ids::{SessionId, TabId};
use koshi_core::process::PtySize;
use koshi_pane::pane::state::PaneRecord;
use koshi_pty::backend::state::PtyBackend;
use koshi_session::client::{Client, ClientOrigin, ClientRegistry};
use koshi_session::session::state::{Session, Tab};
use koshi_terminal::engine::TerminalEngine;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::runtime::event::RuntimeEvent;
use crate::runtime::render_schedule::FRAME_INTERVAL_DURATION;

/// A runtime holding one session, one attached client, and one 1-row terminal
/// engine for a pane — a 1-row screen so each fed newline pushes exactly one
/// line into scrollback. Returns the runtime plus the pane and client ids.
fn build_runtime_with_pane() -> (Server, PaneId, ClientId) {
    let (runtime_event_sender, inbox_receiver) = mpsc::channel::<RuntimeEvent>();
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender),
    )));
    let mut runtime = Server::from_runtime_parts(pty_backend, inbox_receiver);
    let (pane_id, client_id) = attach_test_session_with_pane(&mut runtime);
    (runtime, pane_id, client_id)
}

/// Add one more session to `runtime`, holding one attached client, one tab, one pane,
/// and a 1-row terminal engine for that pane. Returns the new pane and client
/// ids.
fn attach_test_session_with_pane(runtime: &mut Server) -> (PaneId, ClientId) {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();

    let mut session = Session::from_identity_and_client_registry(
        session_id,
        "s".to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(pane_id))
        .expect("unique pane id");
    session.tabs.insert(
        tab_id,
        Tab::from_root_pane(tab_id, "t".to_string(), 0, pane_id),
    );
    let mut client = Client::from_attachment(
        client_id,
        session_id,
        SystemTime::now(),
        Size {
            column_count: 8,
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
    runtime.session_by_id.insert(session_id, session);

    runtime.terminal_engine_by_pane_id.insert(
        pane_id,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 8,
            row_count: 1,
        }),
    );

    (pane_id, client_id)
}

/// Add one more pane, with its own 1-row terminal engine, to the session that
/// already owns `sibling_pane_id`. Returns the new pane id.
fn add_pane_beside(runtime: &mut Server, sibling_pane_id: PaneId) -> PaneId {
    let pane_id = PaneId::new();
    let session_id = *runtime
        .list_sessions()
        .iter()
        .find(|(_, session)| {
            session
                .panes
                .get_pane_record_by_id(sibling_pane_id)
                .is_some()
        })
        .expect("the sibling pane is in a session")
        .0;
    runtime
        .session_by_id
        .get_mut(&session_id)
        .unwrap()
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(pane_id))
        .expect("unique pane id");
    runtime.terminal_engine_by_pane_id.insert(
        pane_id,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 8,
            row_count: 1,
        }),
    );
    pane_id
}

/// The attached client under `client_id`, found in whichever session holds it.
fn get_client_for_id(runtime: &Server, client_id: ClientId) -> &Client {
    runtime
        .list_sessions()
        .values()
        .find_map(|session| session.clients.get_client_by_id(client_id))
        .expect("the client is attached")
}

/// The client's current scroll offset for the pane.
fn get_scroll_offset(runtime: &Server, client_id: ClientId, pane_id: PaneId) -> usize {
    get_client_for_id(runtime, client_id).get_scroll_offset(pane_id)
}

/// Whether the client's view of the pane is held against live output.
fn is_view_held(runtime: &Server, client_id: ClientId, pane_id: PaneId) -> bool {
    get_client_for_id(runtime, client_id).is_view_held(pane_id)
}

/// Put the client in visual mode with a highlight in the pane, as the mouse layer
/// does on a drag. The highlight sits on row 0 — the oldest line — which the view
/// rules do not care about, until an erase drops every line under it.
fn set_highlight(runtime: &mut Server, client_id: ClientId, pane_id: PaneId) {
    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 0,
            column_index: 4,
        },
    };
    runtime
        .get_client_mut(client_id)
        .unwrap()
        .set_selection(pane_id, selection);
}

/// Leave visual mode in `pane_id`, as a click or any non-copy key landing in it does.
fn clear_highlight(runtime: &mut Server, client_id: ClientId, pane_id: PaneId) {
    runtime
        .get_client_mut(client_id)
        .unwrap()
        .clear_selection(pane_id);
}

/// The pane engine's current retained scrollback length.
fn get_retained_line_count(runtime: &Server, pane_id: PaneId) -> usize {
    runtime
        .terminal_engine_by_pane_id
        .get(&pane_id)
        .unwrap()
        .get_terminal_state()
        .get_scrollback()
        .get_retained_line_count()
}

#[test]
fn scroll_up_moves_into_history_and_clamps_at_the_oldest_line() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n\n\n"); // five lines into scrollback
    assert_eq!(get_retained_line_count(&runtime, pane_id), 5);

    runtime.scroll_up(client_id, pane_id, 3);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 3);
    assert!(is_view_held(&runtime, client_id, pane_id)); // scrolled up at all: held

    runtime.scroll_up(client_id, pane_id, 10); // clamps to the retained count
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 5);
    assert!(is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn scroll_down_returns_toward_live_and_follows_again_at_the_bottom() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n\n\n");
    runtime.scroll_up(client_id, pane_id, 5);

    runtime.scroll_down(client_id, pane_id, 2);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 3);
    assert!(is_view_held(&runtime, client_id, pane_id)); // stopped short: still held

    runtime.scroll_down(client_id, pane_id, 10); // saturates at the newest line
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 0);
    assert!(!is_view_held(&runtime, client_id, pane_id)); // no highlight: follows live again
}

#[test]
fn scrolling_to_the_bottom_in_visual_mode_keeps_the_view_held() {
    // Scrolling does not end visual mode: back at the newest line with a
    // highlight up, the view is still held, and the next output raises it.
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    set_highlight(&mut runtime, client_id, pane_id);
    runtime.scroll_up(client_id, pane_id, 2);

    runtime.scroll_to_bottom(client_id, pane_id);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 0);
    assert!(is_view_held(&runtime, client_id, pane_id)); // the highlight still holds it

    runtime.handle_pty_output(pane_id, b"\n\n"); // output arrives under the highlight
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 2); // the view rose with its text
    assert!(is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn scroll_up_by_the_maximum_and_scroll_to_bottom_jump_to_the_ends() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n\n"); // four lines

    runtime.scroll_up(client_id, pane_id, usize::MAX);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 4);
    assert!(is_view_held(&runtime, client_id, pane_id));

    runtime.scroll_to_bottom(client_id, pane_id);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 0);
    assert!(!is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn new_output_anchors_a_scrolled_back_view_to_the_same_history() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n"); // three lines
    runtime.scroll_up(client_id, pane_id, 2);

    runtime.handle_pty_output(pane_id, b"\n\n"); // two more pushed
                                                 // The offset rose by the two pushed lines, and the same history stays in view.
    assert_eq!(get_retained_line_count(&runtime, pane_id), 5);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 4);
}

#[test]
fn new_output_leaves_a_live_following_view_following() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 0); // never scrolled
    assert!(!is_view_held(&runtime, client_id, pane_id));

    runtime.handle_pty_output(pane_id, b"\n\n"); // more output
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 0); // still following live
    assert!(!is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn new_output_holds_a_highlighted_view_at_the_bottom_on_the_same_lines() {
    // A highlight at the newest line rises with its text as output pushes: two
    // pushed lines take the offset from 0 to 2.
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    set_highlight(&mut runtime, client_id, pane_id);

    runtime.handle_pty_output(pane_id, b"\n\n"); // two lines pushed under it
    assert_eq!(get_retained_line_count(&runtime, pane_id), 5);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 2); // rose by the two pushed lines
    assert!(is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn leaving_visual_mode_at_the_bottom_returns_the_view_to_live_output() {
    // A highlight made at the newest line and dropped before any output moved
    // the view leaves offset 0 and nothing holding it, so the view follows live
    // output again.
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    set_highlight(&mut runtime, client_id, pane_id);
    assert!(is_view_held(&runtime, client_id, pane_id));

    clear_highlight(&mut runtime, client_id, pane_id); // a click elsewhere, or any non-copy key
    assert!(!is_view_held(&runtime, client_id, pane_id));

    runtime.handle_pty_output(pane_id, b"\n\n"); // output arrives
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 0); // followed it down
    assert!(!is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn leaving_visual_mode_leaves_a_view_that_output_pushed_up_held() {
    // Output pushes the view 2 lines up while the highlight is up. Dropping the
    // highlight leaves offset 2, which holds the view on its own.
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    set_highlight(&mut runtime, client_id, pane_id);
    runtime.handle_pty_output(pane_id, b"\n\n");
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 2);

    clear_highlight(&mut runtime, client_id, pane_id);
    assert!(is_view_held(&runtime, client_id, pane_id)); // still 2 lines up

    runtime.scroll_to_bottom(client_id, pane_id); // the user scrolls back down
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 0);
    assert!(!is_view_held(&runtime, client_id, pane_id)); // and follows live again
}

#[test]
fn output_keeps_arriving_while_a_view_is_held() {
    // A held view holds the view, not the pane: the child's output still reaches
    // the engine and still fills the scrollback underneath.
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    runtime.scroll_up(client_id, pane_id, 3);

    runtime.handle_pty_output(pane_id, b"\n\n\n\n");
    assert_eq!(get_retained_line_count(&runtime, pane_id), 7); // history kept growing under the held view
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 7);
    assert!(is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn erasing_the_scrollback_returns_a_scrolled_view_to_live_output() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    runtime.scroll_up(client_id, pane_id, 3);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 3);

    // ED 3 erases the scrollback: the offset reclamps to 0, and with no highlight
    // there is nothing else holding the view, so it follows live again.
    runtime.handle_pty_output(pane_id, b"\x1b[3J");
    assert_eq!(get_retained_line_count(&runtime, pane_id), 0);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 0);
    assert!(!is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn erasing_the_scrollback_leaves_a_live_screen_highlight_held() {
    // ED 3 erases the scrollback and leaves the live screen alone: a highlight on
    // the live screen still names retained lines, so it is kept and keeps holding
    // the view, while the offset reclamps to 0.
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    // Three lines pushed, so the live screen's top row is line 3.
    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 3,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 3,
            column_index: 4,
        },
    };
    runtime
        .get_client_mut(client_id)
        .unwrap()
        .set_selection(pane_id, selection);

    runtime.handle_pty_output(pane_id, b"\x1b[3J");
    assert_eq!(get_retained_line_count(&runtime, pane_id), 0);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 0);
    assert!(is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn evicting_a_highlights_lines_leaves_the_view_scrolled_up() {
    // A highlight holds the view, so output raises the offset under it. Once the cap
    // evicts every line the highlight names, the highlight is dropped and the
    // offset stays: the view is held by the offset alone and stays
    // until the client scrolls down.
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    set_highlight(&mut runtime, client_id, pane_id); // row 0, in history

    // First storm: the held view's offset rises with the output.
    runtime.handle_pty_output(pane_id, &b"\n".repeat(5_000));
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 5_000);
    assert!(is_view_held(&runtime, client_id, pane_id));

    // Second storm pushes past the 10 000-line cap: row 0 is evicted.
    runtime.handle_pty_output(pane_id, &b"\n".repeat(6_000));
    assert_eq!(
        runtime
            .get_client_mut(client_id)
            .unwrap()
            .get_selection(pane_id),
        None,
        "the highlight's lines are gone, so it is dropped"
    );
    assert_eq!(
        get_scroll_offset(&runtime, client_id, pane_id),
        10_000,
        "the offset stays, clamped to the oldest retained line"
    );
    assert!(
        is_view_held(&runtime, client_id, pane_id),
        "held by the offset now — an ordinary scrolled-up view"
    );

    // Scrolling down by hand returns to live, like any scrolled-up view.
    runtime.scroll_to_bottom(client_id, pane_id);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 0);
    assert!(!is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn erasing_the_scrollback_drops_a_highlight_that_lived_only_there() {
    // ED 3 removes every line the helper's row-0 highlight names, so the
    // highlight is dropped and nothing holds the view.
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    set_highlight(&mut runtime, client_id, pane_id);

    runtime.handle_pty_output(pane_id, b"\x1b[3J");
    assert_eq!(get_retained_line_count(&runtime, pane_id), 0);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 0);
    assert!(!is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn a_no_op_scroll_schedules_no_repaint() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n"); // two retained lines
    let now = Instant::now();
    assert!(runtime.render_scheduler.claim_due_render(now)); // drain the output invalidation

    runtime.scroll_down(client_id, pane_id, 1); // already following live
    runtime.scroll_up(client_id, pane_id, 0); // zero-line move
    runtime.scroll_up(ClientId::new(), pane_id, 1); // unknown client
                                                    // Nothing marked the frame stale: no wakeup is due.
    assert_eq!(runtime.render_scheduler.compute_next_wakeup(now), None);

    runtime.scroll_up(client_id, pane_id, 1); // a real move marks the frame stale
    assert_eq!(
        runtime.render_scheduler.compute_next_wakeup(now),
        Some(FRAME_INTERVAL_DURATION)
    );
}

#[test]
fn scrolling_a_pane_with_no_terminal_moves_nothing() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n");
    let now = Instant::now();
    assert!(runtime.render_scheduler.claim_due_render(now)); // drain the output invalidation

    let engineless_pane_id = PaneId::new(); // never had a terminal engine
    runtime.scroll_up(client_id, engineless_pane_id, 3);
    runtime.scroll_up(client_id, engineless_pane_id, usize::MAX);
    runtime.scroll_down(client_id, engineless_pane_id, 3);
    assert_eq!(
        get_scroll_offset(&runtime, client_id, engineless_pane_id),
        0
    );
    assert!(!is_view_held(&runtime, client_id, engineless_pane_id));
    assert_eq!(runtime.render_scheduler.compute_next_wakeup(now), None);
}

#[test]
fn scrolling_down_as_an_unknown_client_moves_nothing() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    runtime.scroll_up(client_id, pane_id, 2);
    let now = Instant::now();
    assert!(runtime.render_scheduler.claim_due_render(now));

    runtime.scroll_down(ClientId::new(), pane_id, 1);
    runtime.scroll_to_bottom(ClientId::new(), pane_id);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 2); // the attached client is untouched
    assert!(is_view_held(&runtime, client_id, pane_id));
    assert_eq!(runtime.render_scheduler.compute_next_wakeup(now), None);
}

#[test]
fn scrolling_by_zero_lines_leaves_a_scrolled_view_where_it_is() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    runtime.scroll_up(client_id, pane_id, 2);
    let now = Instant::now();
    assert!(runtime.render_scheduler.claim_due_render(now));

    runtime.scroll_up(client_id, pane_id, 0);
    runtime.scroll_down(client_id, pane_id, 0);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 2);
    assert!(is_view_held(&runtime, client_id, pane_id));
    assert_eq!(runtime.render_scheduler.compute_next_wakeup(now), None);
}

#[test]
fn scroll_up_by_the_maximum_with_no_history_stays_at_the_newest_line() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    let now = Instant::now();

    runtime.scroll_up(client_id, pane_id, usize::MAX); // nothing has scrolled off the screen yet
    assert_eq!(get_retained_line_count(&runtime, pane_id), 0);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 0);
    assert!(!is_view_held(&runtime, client_id, pane_id));
    assert_eq!(runtime.render_scheduler.compute_next_wakeup(now), None);
}

#[test]
fn scrolling_to_the_bottom_twice_schedules_one_repaint() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    runtime.scroll_up(client_id, pane_id, 2);
    let now = Instant::now();
    assert!(runtime.render_scheduler.claim_due_render(now));

    runtime.scroll_to_bottom(client_id, pane_id);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 0);
    assert_eq!(
        runtime.render_scheduler.compute_next_wakeup(now),
        Some(FRAME_INTERVAL_DURATION)
    );
    let painted_at = now + FRAME_INTERVAL_DURATION;
    assert!(runtime.render_scheduler.claim_due_render(painted_at));

    runtime.scroll_to_bottom(client_id, pane_id); // already at the newest line
    assert_eq!(
        runtime.render_scheduler.compute_next_wakeup(painted_at),
        None
    );
}

#[test]
fn scrolling_one_pane_leaves_the_clients_other_pane_at_the_newest_line() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    let other_pane_id = add_pane_beside(&mut runtime, pane_id);
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    runtime.handle_pty_output(other_pane_id, b"\n\n\n");

    runtime.scroll_up(client_id, pane_id, 2);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 2);
    assert!(is_view_held(&runtime, client_id, pane_id));
    assert_eq!(get_scroll_offset(&runtime, client_id, other_pane_id), 0);
    assert!(!is_view_held(&runtime, client_id, other_pane_id));

    // Output in the other pane re-anchors only the views held in that pane.
    runtime.handle_pty_output(other_pane_id, b"\n\n");
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 2);
    assert_eq!(get_scroll_offset(&runtime, client_id, other_pane_id), 0);
}

#[test]
fn scrolling_reaches_a_client_in_a_second_session() {
    let (mut runtime, first_pane_id, first_client_id) = build_runtime_with_pane();
    let (second_pane_id, second_client_id) = attach_test_session_with_pane(&mut runtime);
    runtime.handle_pty_output(first_pane_id, b"\n\n\n");
    runtime.handle_pty_output(second_pane_id, b"\n\n\n\n");

    runtime.scroll_up(second_client_id, second_pane_id, 3);
    assert_eq!(
        get_scroll_offset(&runtime, second_client_id, second_pane_id),
        3
    );
    assert!(is_view_held(&runtime, second_client_id, second_pane_id));
    assert_eq!(
        get_scroll_offset(&runtime, first_client_id, first_pane_id),
        0
    ); // the first_client_id session is untouched
    assert!(!is_view_held(&runtime, first_client_id, first_pane_id));

    // Output in the second session's pane re-anchors that session's client alone.
    runtime.handle_pty_output(second_pane_id, b"\n\n");
    assert_eq!(
        get_scroll_offset(&runtime, second_client_id, second_pane_id),
        5
    );
    assert_eq!(
        get_scroll_offset(&runtime, first_client_id, first_pane_id),
        0
    );
}

#[test]
fn anchor_clamps_a_held_view_to_the_surviving_lines() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    runtime.scroll_up(client_id, pane_id, 2);

    // A heavy-truncation frame: five lines pushed, only two survive. The anchor
    // lands the view on the oldest surviving line rather than past it.
    runtime.anchor_held_views(pane_id, 5, 2);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 2);
    assert!(is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn anchoring_with_no_pushed_lines_reclamps_a_held_view_to_the_shorter_history() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    runtime.scroll_up(client_id, pane_id, 3);

    // A frame that pushed nothing and left one line: the held view drops onto it.
    runtime.anchor_held_views(pane_id, 0, 1);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 1);
    assert!(is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn anchoring_a_pane_in_no_session_changes_nothing() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    runtime.scroll_up(client_id, pane_id, 2);

    runtime.anchor_held_views(PaneId::new(), 4, 9); // a pane_id no session owns
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 2);
    assert!(is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn an_erase_and_new_output_in_one_chunk_reanchors_exactly() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n"); // three retained lines
    runtime.scroll_up(client_id, pane_id, 2);

    // One chunk: ED 3 erases the history, then four lines push fresh history.
    // The monotonic push counter keeps the count exact across the erase, so the
    // held view rises by all four and clamps to the surviving lines.
    runtime.handle_pty_output(pane_id, b"\x1b[3J\n\n\n\n");
    assert_eq!(get_retained_line_count(&runtime, pane_id), 4);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 4);
    assert!(is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn output_that_touches_no_history_leaves_a_held_view_alone() {
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    runtime.scroll_up(client_id, pane_id, 2);

    runtime.handle_pty_output(pane_id, b"hi"); // prints on the live row, pushes nothing
    assert_eq!(get_retained_line_count(&runtime, pane_id), 3);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 2);
    assert!(is_view_held(&runtime, client_id, pane_id));
}

#[test]
fn output_re_anchors_each_client_on_a_shared_pane_on_its_own() {
    // Three clients on one pane — one scrolled up, one holding a highlight at the
    // bottom, one following. The view is per-client, so each is re-anchored alone
    // and none disturbs another.
    let (mut runtime, pane_id, first_client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");

    let session_id = *runtime.list_sessions().keys().next().unwrap();
    let tab_id = runtime
        .get_client_mut(first_client_id)
        .unwrap()
        .get_active_tab_id();
    let (second_client_id, third_client_id) = (ClientId::new(), ClientId::new());
    for client_id in [second_client_id, third_client_id] {
        let client = Client::from_attachment(
            client_id,
            session_id,
            SystemTime::now(),
            Size {
                column_count: 8,
                row_count: 1,
            },
            None,
            tab_id,
            ClientOrigin::Local,
            "C-test-client".to_string(),
            0,
        );
        runtime
            .session_by_id
            .get_mut(&session_id)
            .unwrap()
            .attach_client(client);
    }

    runtime.scroll_up(first_client_id, pane_id, 2);
    set_highlight(&mut runtime, second_client_id, pane_id);
    // `third_client_id` is left following live.

    runtime.handle_pty_output(pane_id, b"\n\n"); // two lines pushed

    assert_eq!(get_scroll_offset(&runtime, first_client_id, pane_id), 4); // rose by two, still held
    assert!(is_view_held(&runtime, first_client_id, pane_id));
    assert_eq!(get_scroll_offset(&runtime, second_client_id, pane_id), 2); // rose by two from the bottom
    assert!(is_view_held(&runtime, second_client_id, pane_id));
    assert_eq!(get_scroll_offset(&runtime, third_client_id, pane_id), 0); // followed live, untouched
    assert!(!is_view_held(&runtime, third_client_id, pane_id));
}

/// The engine's effective view offset for the pane — what the renderer actually
/// shows, which is `0` on the alternate screen however far the stored offset sits.
fn compute_effective_offset(
    runtime: &Server,
    pane_id: PaneId,
    stored_scroll_offset: usize,
) -> usize {
    runtime
        .terminal_engine_by_pane_id
        .get(&pane_id)
        .unwrap()
        .get_terminal_state()
        .compute_effective_view_offset(stored_scroll_offset)
}

#[test]
fn scrolling_up_on_the_alternate_screen_moves_the_stored_offset_but_shows_live() {
    // The alternate screen keeps no history of its own, but the pane's one scrollback survives
    // entering it (it is restored on exit), so `scroll_up` still clamps against those
    // retained lines and moves the stored offset. The renderer never
    // shows it there, though: the engine's effective offset is 0 on the alternate
    // screen.
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n"); // three retained lines
    runtime.handle_pty_output(pane_id, b"\x1b[?1049h"); // enter the alternate screen
    assert_eq!(
        get_retained_line_count(&runtime, pane_id),
        3,
        "the primary scrollback survives"
    );

    runtime.scroll_up(client_id, pane_id, 2);
    // The stored offset moved — `scroll_up` clamps to the retained count,
    // which the alternate screen did not clear.
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 2);
    // But nothing scrolled on screen: the effective offset is 0 on the alt screen.
    assert_eq!(compute_effective_offset(&runtime, pane_id, 2), 0);
}

#[test]
fn scroll_up_by_the_maximum_on_the_alternate_screen_clamps_to_the_retained_history() {
    // On the alternate screen a `scroll_up` by the maximum still lands exactly
    // on the retained primary line count, and still shows nothing scrolled.
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n\n"); // four retained lines
    runtime.handle_pty_output(pane_id, b"\x1b[?1049h");

    runtime.scroll_up(client_id, pane_id, usize::MAX);
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 4); // clamped to the retained count
    assert_eq!(compute_effective_offset(&runtime, pane_id, 4), 0); // still live on screen
}

#[test]
fn a_view_scrolled_while_on_the_alternate_screen_applies_once_it_exits() {
    // The stored offset moves while the alternate screen hides it, and leaving
    // the alternate screen shows it: the primary view sits back by what was
    // scrolled while the full-screen program was up.
    let (mut runtime, pane_id, client_id) = build_runtime_with_pane();
    runtime.handle_pty_output(pane_id, b"\n\n\n");
    runtime.handle_pty_output(pane_id, b"\x1b[?1049h"); // enter
    runtime.scroll_up(client_id, pane_id, 2);
    assert_eq!(compute_effective_offset(&runtime, pane_id, 2), 0); // hidden while on the alt screen

    runtime.handle_pty_output(pane_id, b"\x1b[?1049l"); // leave the alternate screen
    assert_eq!(
        get_retained_line_count(&runtime, pane_id),
        3,
        "the primary history is back"
    );
    // The stored offset is unchanged and now shows: the primary view sits two
    // lines back.
    assert_eq!(get_scroll_offset(&runtime, client_id, pane_id), 2);
    assert_eq!(compute_effective_offset(&runtime, pane_id, 2), 2);
}
