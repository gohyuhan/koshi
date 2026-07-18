//! Tests for selecting text with the mouse: what each gesture highlights, which
//! pane it lands on, and the view scrolling while a drag is held past an edge.

use super::*;

use std::sync::{mpsc, Arc};

#[cfg(feature = "native")]
use std::sync::Mutex;

use koshi_config::types::ClipboardBackend;
use koshi_core::command::{Command, CommandEnvelope, CommandSource, NewPaneArgs, SelectionKind};
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{CommandId, TabId};
use koshi_core::key::{Key, KeyChord, ModFlags};
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_test_support::fake_pty::FakePtyBackend;
use std::time::SystemTime;

use crate::placeholder::{NullSnapshotProvider, NullStorage};

#[cfg(feature = "native")]
use crate::runtime::clipboard::ClipboardWriter;

#[cfg(feature = "native")]
struct RecordingClipboard {
    writes: Arc<Mutex<Vec<String>>>,
}

#[cfg(feature = "native")]
impl ClipboardWriter for RecordingClipboard {
    fn write(&mut self, text: &str) -> bool {
        self.writes
            .lock()
            .expect("recording lock")
            .push(text.to_owned());
        true
    }
}

#[cfg(feature = "native")]
struct FailingClipboard;

#[cfg(feature = "native")]
impl ClipboardWriter for FailingClipboard {
    fn write(&mut self, _text: &str) -> bool {
        false
    }
}

/// A runtime with one bootstrapped 80x24 client and its single pane.
fn runtime() -> (Runtime, ClientId, PaneId) {
    let fake = Arc::new(FakePtyBackend::new());
    let (tx, rx) = mpsc::channel();
    let mut rt = Runtime::new(
        fake,
        Arc::new(NullSnapshotProvider),
        Arc::new(NullStorage),
        rx,
        tx,
        TerminalCleanupGuard::new(),
        Direction::Right,
    );
    let client = rt
        .bootstrap_local(Size { cols: 80, rows: 24 }, SystemTime::UNIX_EPOCH)
        .expect("bootstrap");
    let pane = *rt.pty_handles.keys().next().expect("one pane");
    (rt, client, pane)
}

/// Feed `bytes` into `pane`'s terminal, so its screen has text to select.
fn feed(rt: &mut Runtime, pane: PaneId, bytes: &[u8]) {
    rt.handle_pty_output(pane, bytes);
}

/// The screen origin of `pane`'s content area: the cell its row 0, column 0
/// draws at.
fn origin(rt: &Runtime, client: ClientId, pane: PaneId) -> Point {
    let snapshot = rt.build_snapshot(client).expect("snapshot");
    koshi_renderer::pane_content_rect(&snapshot, pane)
        .expect("content rect")
        .origin
}

/// The screen cell for `pane`'s content row `row`, column `col`.
fn cell_at(rt: &Runtime, client: ClientId, pane: PaneId, col: u16, row: u16) -> Point {
    let origin = origin(rt, client, pane);
    Point {
        x: origin.x + col,
        y: origin.y + row,
    }
}

/// `pane`'s last content column. Derived, not assumed: the pane's border ring
/// eats into the client's viewport, so a pane in an 80-column terminal is
/// narrower than 80.
fn last_col(rt: &Runtime, client: ClientId, pane: PaneId) -> u16 {
    let snapshot = rt.build_snapshot(client).expect("snapshot");
    koshi_renderer::pane_content_rect(&snapshot, pane)
        .expect("content rect")
        .size
        .cols
        - 1
}

fn press_at(at: Point) -> MouseInput {
    MouseInput {
        kind: MouseKind::Press(MouseButton::Left),
        at,
        mods: ModFlags::NONE,
    }
}

fn alt_press_at(at: Point) -> MouseInput {
    MouseInput {
        kind: MouseKind::Press(MouseButton::Left),
        at,
        mods: ModFlags::ALT,
    }
}

fn drag_at(at: Point) -> MouseInput {
    MouseInput {
        kind: MouseKind::Drag(MouseButton::Left),
        at,
        mods: ModFlags::NONE,
    }
}

fn release_at(at: Point) -> MouseInput {
    MouseInput {
        kind: MouseKind::Release(MouseButton::Left),
        at,
        mods: ModFlags::NONE,
    }
}

fn select_hello(rt: &mut Runtime, client: ClientId, pane: PaneId) {
    let mut clock = Clock::new();
    let from = cell_at(rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(rt, client, pane, 4, 0)),
        clock.tick(),
    );
    rt.handle_mouse_input(
        client,
        release_at(cell_at(rt, client, pane, 4, 0)),
        clock.tick(),
    );
}

/// A clock whose every reading is a second after the last, so no two presses
/// fall inside the click threshold unless a test asks them to.
struct Clock(Instant);

impl Clock {
    fn new() -> Self {
        Clock(Instant::now())
    }

    /// A second on from the last reading: two presses apart.
    fn tick(&mut self) -> Instant {
        self.0 += Duration::from_secs(1);
        self.0
    }

    /// A tenth of a second on: inside the 400ms threshold, so a second press
    /// here is a double click.
    fn quick(&mut self) -> Instant {
        self.0 += Duration::from_millis(100);
        self.0
    }
}

/// This client's highlight in `pane`.
fn selection(rt: &mut Runtime, client: ClientId, pane: PaneId) -> Option<Selection> {
    rt.client_mut(client).expect("client").selection(pane)
}

/// Split the focused pane and return the new pane's id.
fn split(rt: &mut Runtime, client: ClientId) -> PaneId {
    let before: Vec<PaneId> = rt.pty_handles.keys().copied().collect();
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client),
        SystemTime::now(),
        Command::NewPane(NewPaneArgs::default()),
    );
    let _ = rt.dispatch(envelope);
    *rt.pty_handles
        .keys()
        .find(|id| !before.contains(id))
        .expect("a new pane")
}

#[test]
fn a_drag_highlights_from_the_press_to_the_pointer() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 6, 0); // the `w`
    let to = cell_at(&rt, client, pane, 10, 0); // the `d`
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(client, drag_at(to), clock.tick());

    let selection = selection(&mut rt, client, pane).expect("a highlight");
    assert_eq!(selection.kind, SelectionKind::Character);
    assert_eq!(selection.anchor, GridPos { row: 0, col: 6 });
    assert_eq!(selection.cursor, GridPos { row: 0, col: 10 });
}

#[test]
fn a_press_with_no_drag_leaves_no_highlight() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    // A plain click: press and release, the pointer never moving. Nothing is
    // highlighted, and in particular no empty highlight is left to hold the view.
    let at = cell_at(&rt, client, pane, 3, 0);
    rt.handle_mouse_input(client, press_at(at), clock.tick());
    rt.handle_mouse_input(client, release_at(at), clock.tick());

    assert_eq!(selection(&mut rt, client, pane), None);
    assert!(
        !rt.client_mut(client).expect("client").is_view_held(pane),
        "a click leaves the view following live output"
    );
}

#[test]
fn a_press_drops_the_highlight_that_was_up() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    let to = cell_at(&rt, client, pane, 4, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(client, drag_at(to), clock.tick());
    assert!(selection(&mut rt, client, pane).is_some(), "highlighted");

    // Clicking again clears it, the way clicking off a selection does in an
    // editor or a browser.
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    assert_eq!(selection(&mut rt, client, pane), None);
}

#[test]
fn a_drag_in_one_pane_leaves_the_other_panes_highlight_alone() {
    let (mut rt, client, first) = runtime();
    let mut clock = Clock::new();
    let second = split(&mut rt, client);
    feed(&mut rt, first, b"first pane");
    feed(&mut rt, second, b"second pane");

    // Highlight in the second pane (the split focused it).
    let from = cell_at(&rt, client, second, 0, 0);
    let to = cell_at(&rt, client, second, 5, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(client, drag_at(to), clock.tick());
    let second_selection = selection(&mut rt, client, second).expect("second pane highlighted");

    // Now focus the first pane and highlight there. A click on an unfocused pane
    // only focuses, so it takes a second press to start the drag.
    let first_from = cell_at(&rt, client, first, 0, 0);
    let first_to = cell_at(&rt, client, first, 4, 0);
    rt.handle_mouse_input(client, press_at(first_from), clock.tick());
    rt.handle_mouse_input(client, press_at(first_from), clock.tick());
    rt.handle_mouse_input(client, drag_at(first_to), clock.tick());

    assert!(
        selection(&mut rt, client, first).is_some(),
        "the first pane is highlighted"
    );
    assert_eq!(
        selection(&mut rt, client, second),
        Some(second_selection),
        "the second pane's highlight is exactly as it was"
    );
}

#[test]
fn a_focus_click_on_another_pane_clears_no_highlight() {
    let (mut rt, client, first) = runtime();
    let mut clock = Clock::new();
    let second = split(&mut rt, client);
    feed(&mut rt, first, b"first pane");
    feed(&mut rt, second, b"second pane");

    let from = cell_at(&rt, client, second, 0, 0);
    let to = cell_at(&rt, client, second, 5, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(client, drag_at(to), clock.tick());
    let held = selection(&mut rt, client, second).expect("second pane highlighted");

    // One click on the other pane: a koshi focus trigger, which never reaches
    // the highlighted pane's program, so it clears nothing.
    rt.handle_mouse_input(
        client,
        press_at(cell_at(&rt, client, first, 0, 0)),
        clock.tick(),
    );

    assert_eq!(
        selection(&mut rt, client, second),
        Some(held),
        "focusing away leaves the highlight up"
    );
}

#[test]
fn a_double_click_drag_snaps_to_whole_words() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    // Two presses inside the threshold, then a drag: word selection.
    let from = cell_at(&rt, client, pane, 2, 0); // inside `hello`
    let to = cell_at(&rt, client, pane, 8, 0); // inside `world`
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(client, press_at(from), clock.quick());
    rt.handle_mouse_input(client, drag_at(to), clock.quick());

    let selection = selection(&mut rt, client, pane).expect("a highlight");
    assert_eq!(selection.kind, SelectionKind::Word);
    assert_eq!(
        selection.anchor,
        GridPos { row: 0, col: 0 },
        "the anchor fell back to the start of `hello`"
    );
    assert_eq!(
        selection.cursor,
        GridPos { row: 0, col: 10 },
        "and the cursor ran on to the end of `world`"
    );
}

#[test]
fn a_triple_click_drag_snaps_to_whole_lines() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let at = cell_at(&rt, client, pane, 4, 0);
    rt.handle_mouse_input(client, press_at(at), clock.tick());
    rt.handle_mouse_input(client, press_at(at), clock.quick());
    rt.handle_mouse_input(client, press_at(at), clock.quick());
    rt.handle_mouse_input(client, drag_at(at), clock.quick());

    let edge = last_col(&rt, client, pane);
    let selection = selection(&mut rt, client, pane).expect("a highlight");
    assert_eq!(selection.kind, SelectionKind::Line);
    assert_eq!(selection.anchor, GridPos { row: 0, col: 0 });
    assert_eq!(
        selection.cursor,
        GridPos { row: 0, col: edge },
        "a line selection runs to the last column"
    );
}

#[test]
fn a_fourth_quick_click_starts_the_run_over() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let at = cell_at(&rt, client, pane, 4, 0);
    for _ in 0..4 {
        rt.handle_mouse_input(client, press_at(at), clock.quick());
    }
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 6, 0)),
        clock.quick(),
    );

    assert_eq!(
        selection(&mut rt, client, pane).expect("a highlight").kind,
        SelectionKind::Character,
        "the run wraps back to a single click after three"
    );
}

#[test]
fn two_slow_clicks_are_two_single_clicks() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    // A second apart: well past the 400ms threshold, so this is not a double
    // click and the drag selects characters, not the whole word.
    let at = cell_at(&rt, client, pane, 2, 0);
    rt.handle_mouse_input(client, press_at(at), clock.tick());
    rt.handle_mouse_input(client, press_at(at), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 3, 0)),
        clock.tick(),
    );

    assert_eq!(
        selection(&mut rt, client, pane).expect("a highlight").kind,
        SelectionKind::Character
    );
}

#[test]
fn alt_held_at_the_press_makes_a_block() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"one\r\ntwo\r\nthree");

    let from = cell_at(&rt, client, pane, 1, 0);
    let to = cell_at(&rt, client, pane, 2, 2);
    rt.handle_mouse_input(client, alt_press_at(from), clock.tick());
    rt.handle_mouse_input(client, drag_at(to), clock.tick());

    let selection = selection(&mut rt, client, pane).expect("a highlight");
    assert_eq!(selection.kind, SelectionKind::Block);
    assert_eq!(selection.anchor, GridPos { row: 0, col: 1 });
    assert_eq!(selection.cursor, GridPos { row: 2, col: 2 });
}

#[test]
fn alt_wins_over_a_double_click() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    // A rectangle is a different shape, not a different amount of text, so it
    // does not compete with the run of clicks.
    let at = cell_at(&rt, client, pane, 2, 0);
    rt.handle_mouse_input(client, press_at(at), clock.tick());
    rt.handle_mouse_input(client, alt_press_at(at), clock.quick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 0)),
        clock.quick(),
    );

    assert_eq!(
        selection(&mut rt, client, pane).expect("a highlight").kind,
        SelectionKind::Block
    );
}

#[test]
fn a_release_ends_the_drag_but_leaves_the_highlight() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    let to = cell_at(&rt, client, pane, 4, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(client, drag_at(to), clock.tick());
    rt.handle_mouse_input(client, release_at(to), clock.tick());

    let after_release = selection(&mut rt, client, pane).expect("the highlight stands");
    assert!(
        rt.client_mut(client)
            .expect("client")
            .selection_drag()
            .is_none(),
        "the gesture is over"
    );

    // A later drag with no press behind it extends nothing.
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 8, 0)),
        clock.tick(),
    );
    assert_eq!(
        selection(&mut rt, client, pane),
        Some(after_release),
        "a drag with no press behind it changes nothing"
    );
}

#[test]
fn a_highlight_holds_the_view_at_the_live_bottom() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    let to = cell_at(&rt, client, pane, 4, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(client, drag_at(to), clock.tick());

    assert!(
        rt.client_mut(client).expect("client").is_view_held(pane),
        "a highlight holds the view even at offset 0, which an offset alone \
         cannot express"
    );
}

#[test]
fn a_drag_past_the_bottom_edge_scrolls_and_keeps_extending() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    // Fill well past the 24-row screen so there is history to scroll into.
    for i in 0..60 {
        feed(&mut rt, pane, format!("line{i}\r\n").as_bytes());
    }

    // Scroll up so there is somewhere to scroll back down to, then drag past the
    // bottom edge.
    rt.scroll_up(client, pane, 10);
    let before = rt.client_mut(client).expect("client").scroll_offset(pane);
    assert_eq!(before, 10);

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    let below = Point {
        x: from.x,
        y: origin(&rt, client, pane).y + 100, // far below the pane
    };
    rt.handle_mouse_input(client, drag_at(below), clock.tick());

    // The drag armed the scroll rather than scrolling on the event itself.
    let drag = rt
        .client_mut(client)
        .expect("client")
        .selection_drag()
        .expect("a drag");
    assert!(
        drag.scroll_at.is_some(),
        "a pointer past the bottom edge arms the scroll timer"
    );

    // Firing the timer scrolls one line toward live output and keeps the
    // highlight extending, without any further mouse event.
    rt.expire_selection_scrolls(clock.tick());
    assert_eq!(
        rt.client_mut(client).expect("client").scroll_offset(pane),
        before - 1,
        "one line per firing, toward the pointer"
    );
    assert!(
        selection(&mut rt, client, pane).is_some(),
        "and the highlight keeps up"
    );
}

#[test]
fn a_pointer_back_inside_the_pane_stops_the_scrolling() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    for i in 0..60 {
        feed(&mut rt, pane, format!("line{i}\r\n").as_bytes());
    }
    rt.scroll_up(client, pane, 10);

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    let below = Point {
        x: from.x,
        y: origin(&rt, client, pane).y + 100,
    };
    rt.handle_mouse_input(client, drag_at(below), clock.tick());
    assert!(rt
        .client_mut(client)
        .expect("client")
        .selection_drag()
        .expect("a drag")
        .scroll_at
        .is_some());

    // Back inside: the scroll disarms and the view stops moving on its own.
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 2)),
        clock.tick(),
    );
    assert!(
        rt.client_mut(client)
            .expect("client")
            .selection_drag()
            .expect("a drag")
            .scroll_at
            .is_none(),
        "a pointer inside the pane does not scroll"
    );

    let held = rt.client_mut(client).expect("client").scroll_offset(pane);
    rt.expire_selection_scrolls(clock.tick());
    assert_eq!(
        rt.client_mut(client).expect("client").scroll_offset(pane),
        held,
        "a disarmed drag scrolls nothing when the timer runs"
    );
}

#[test]
fn a_wakeup_is_asked_for_only_while_a_drag_is_held_past_an_edge() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");
    let now = clock.tick();

    assert_eq!(
        rt.next_selection_scroll_wakeup(now),
        None,
        "an idle client asks for no wakeup"
    );

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    assert_eq!(
        rt.next_selection_scroll_wakeup(clock.tick()),
        None,
        "a drag inside the pane asks for no wakeup"
    );

    let below = Point {
        x: from.x,
        y: origin(&rt, client, pane).y + 100,
    };
    rt.handle_mouse_input(client, drag_at(below), clock.tick());
    assert!(
        rt.next_selection_scroll_wakeup(clock.tick()).is_some(),
        "a drag past the edge asks the loop to wake"
    );
}

#[test]
fn switching_to_the_alternate_screen_drops_the_highlight() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    assert!(selection(&mut rt, client, pane).is_some(), "highlighted");

    // The program enters the alternate screen (what `vim` does on start). A row
    // number counts lines pushed into scrollback, which the alternate screen has
    // none of, so the highlight names nothing there.
    feed(&mut rt, pane, b"\x1b[?1049h");

    assert_eq!(
        selection(&mut rt, client, pane),
        None,
        "the highlight went with the screen"
    );
    assert!(
        !rt.client_mut(client).expect("client").is_view_held(pane),
        "and the view is no longer held by it"
    );
}

#[test]
fn a_drag_beyond_the_last_column_clamps_to_the_edge() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    // Far to the right of the pane: there is no more text sideways, so the
    // highlight stops at the last column rather than running on.
    let right_of = Point {
        x: origin(&rt, client, pane).x + 200,
        y: from.y,
    };
    rt.handle_mouse_input(client, drag_at(right_of), clock.tick());

    let edge = last_col(&rt, client, pane);
    let selection = selection(&mut rt, client, pane).expect("a highlight");
    assert_eq!(selection.cursor, GridPos { row: 0, col: edge });
    assert!(
        rt.client_mut(client)
            .expect("client")
            .selection_drag()
            .expect("a drag")
            .scroll_at
            .is_none(),
        "a sideways overshoot does not scroll"
    );
}

#[test]
fn a_drag_up_leaves_the_anchor_after_the_cursor() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"one\r\ntwo\r\nthree");

    // Press on the third row and drag up to the first: the anchor stays where
    // the press was, so it is the later end.
    let from = cell_at(&rt, client, pane, 2, 2);
    let to = cell_at(&rt, client, pane, 1, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(client, drag_at(to), clock.tick());

    let selection = selection(&mut rt, client, pane).expect("a highlight");
    assert_eq!(selection.anchor, GridPos { row: 2, col: 2 });
    assert_eq!(selection.cursor, GridPos { row: 0, col: 1 });
}

#[test]
fn switching_tabs_ends_the_drag_and_keeps_the_highlight() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    let held = selection(&mut rt, client, pane).expect("highlighted");

    rt.client_mut(client)
        .expect("client")
        .update_active_tab(TabId::new());

    assert!(
        rt.client_mut(client)
            .expect("client")
            .selection_drag()
            .is_none(),
        "the drag's pane is no longer on the client's frame"
    );
    assert_eq!(
        selection(&mut rt, client, pane),
        Some(held),
        "the highlight belongs to its pane and is found again on switching back"
    );
}

#[test]
fn output_under_a_highlight_leaves_it_on_the_same_text_and_the_same_screen_row() {
    // The point of numbering rows absolutely: the highlight is stored once and
    // never re-anchored, yet output arriving underneath moves neither the text
    // it names nor where it draws.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"target line\r\n");

    // Highlight `target` on the first row.
    let from = cell_at(&rt, client, pane, 0, 0);
    let to = cell_at(&rt, client, pane, 5, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(client, drag_at(to), clock.tick());
    let before = selection(&mut rt, client, pane).expect("a highlight");
    let drawn_before = drawn_rows(&rt, client);

    // Enough output to push the highlighted line off the top of the screen.
    for i in 0..30 {
        feed(&mut rt, pane, format!("noise{i}\r\n").as_bytes());
    }

    assert_eq!(
        selection(&mut rt, client, pane),
        Some(before),
        "the stored highlight was never touched"
    );
    assert_eq!(
        drawn_rows(&rt, client),
        drawn_before,
        "and it still draws on the same screen row: the view was held, so the \
         text under it did not move"
    );
}

/// The highlight rows the client's first pane draws this frame.
fn drawn_rows(rt: &Runtime, client: ClientId) -> Option<Vec<(u16, u16, u16)>> {
    let snap = rt.build_snapshot(client).expect("snapshot");
    snap.panes[0]
        .selection
        .as_ref()
        .map(|spans| spans.rows.clone())
}

#[test]
fn a_word_on_the_alternate_screen_never_reaches_into_the_primarys_history() {
    // The alternate screen keeps no scrollback of its own. Growing a word from
    // its top row must stop there, not walk up into the lines the PRIMARY
    // pushed into history — those are a different screen's text entirely.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();

    // A single very long line on the primary, wrapping many times, so the rows
    // it pushes into history all end SOFT — the text "continues" onto the row
    // below, which is what makes the walk cross the boundary.
    feed(&mut rt, pane, "x".repeat(78 * 40).as_bytes());
    // Enter the alternate screen (which does NOT clear the primary's history)
    // and write a word on it. `ab ` puts a separator before `foo`, so the word
    // genuinely starts at column 3 — anything reaching further left has crossed
    // into text that is not on this screen.
    feed(&mut rt, pane, b"\x1b[?47h");
    feed(&mut rt, pane, b"ab foo bar");

    let top = rt
        .terminal_engines
        .get(&pane)
        .expect("engine")
        .state()
        .scrollback()
        .total_pushed();
    assert!(top > 0, "the primary pushed soft-wrapped rows into history");

    // Double-click drag on `foo`.
    let at = cell_at(&rt, client, pane, 4, 0);
    rt.handle_mouse_input(client, press_at(at), clock.tick());
    rt.handle_mouse_input(client, press_at(at), clock.quick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 5, 0)),
        clock.quick(),
    );

    let selection = selection(&mut rt, client, pane).expect("a highlight");
    assert!(
        selection.anchor.row >= top,
        "the word anchored at row {} — below the alternate screen's first row \
         ({top}), i.e. inside the primary's scrollback",
        selection.anchor.row
    );
    assert_eq!(
        selection.anchor,
        GridPos { row: top, col: 3 },
        "the word starts at the `f` of `foo` on the alternate screen"
    );
}

#[test]
fn a_plain_double_click_selects_the_word_under_the_pointer() {
    // The everyday gesture: double-click a word, no drag at all.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let at = cell_at(&rt, client, pane, 8, 0); // inside `world`
    rt.handle_mouse_input(client, press_at(at), clock.tick());
    rt.handle_mouse_input(client, release_at(at), clock.quick());
    rt.handle_mouse_input(client, press_at(at), clock.quick());
    rt.handle_mouse_input(client, release_at(at), clock.quick());

    let selection = selection(&mut rt, client, pane).expect("`world` is highlighted");
    assert_eq!(selection.kind, SelectionKind::Word);
    assert_eq!(selection.anchor, GridPos { row: 0, col: 6 });
    assert_eq!(selection.cursor, GridPos { row: 0, col: 10 });
}

#[test]
fn a_plain_triple_click_selects_the_line_under_the_pointer() {
    // Same gesture family as the double click: complete without a drag.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let at = cell_at(&rt, client, pane, 4, 0);
    for _ in 0..3 {
        rt.handle_mouse_input(client, press_at(at), clock.quick());
        rt.handle_mouse_input(client, release_at(at), clock.quick());
    }

    let edge = last_col(&rt, client, pane);
    let selection = selection(&mut rt, client, pane).expect("the line is highlighted");
    assert_eq!(selection.kind, SelectionKind::Line);
    assert_eq!(selection.anchor, GridPos { row: 0, col: 0 });
    assert_eq!(selection.cursor, GridPos { row: 0, col: edge });
}

#[test]
fn a_double_click_then_a_drag_extends_from_the_same_word() {
    // The press highlights the word; dragging on keeps extending by whole words
    // from the same anchor, rather than restarting.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let at = cell_at(&rt, client, pane, 2, 0); // inside `hello`
    rt.handle_mouse_input(client, press_at(at), clock.tick());
    rt.handle_mouse_input(client, release_at(at), clock.quick());
    rt.handle_mouse_input(client, press_at(at), clock.quick());
    let after_press = selection(&mut rt, client, pane).expect("`hello` is highlighted");
    assert_eq!(
        after_press.cursor,
        GridPos { row: 0, col: 4 },
        "just `hello`"
    );

    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 8, 0)),
        clock.quick(),
    );
    let after_drag = selection(&mut rt, client, pane).expect("still highlighted");
    assert_eq!(after_drag.anchor, GridPos { row: 0, col: 0 }, "same anchor");
    assert_eq!(
        after_drag.cursor,
        GridPos { row: 0, col: 10 },
        "extended to the end of `world`"
    );
}

#[test]
fn a_double_click_on_empty_space_leaves_no_view_held_over_nothing() {
    // The trap the single-click rule exists to avoid, checked for the gesture
    // that now highlights at the press: a double click on blank cells must not
    // leave a highlight that holds the view with nothing to show.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hi");

    // Column 40 is blank space well past the text.
    let at = cell_at(&rt, client, pane, 40, 0);
    rt.handle_mouse_input(client, press_at(at), clock.tick());
    rt.handle_mouse_input(client, release_at(at), clock.quick());
    rt.handle_mouse_input(client, press_at(at), clock.quick());
    rt.handle_mouse_input(client, release_at(at), clock.quick());

    // Blanks are separators, and a click on a separator selects the run of that
    // character: every blank from the end of `hi` to the row's edge — a real
    // highlight, not an empty one.
    let selection = selection(&mut rt, client, pane).expect("the blank run under the pointer");
    assert_eq!(
        selection.anchor,
        GridPos { row: 0, col: 2 },
        "the run starts right after `hi`"
    );
    assert_eq!(
        selection.cursor,
        GridPos {
            row: 0,
            col: last_col(&rt, client, pane),
        },
        "and reaches the row's edge"
    );
    assert!(
        rt.client_mut(client).expect("client").is_view_held(pane),
        "a real highlight holds the view, and a click clears it again"
    );
}

#[test]
fn a_drag_past_the_top_edge_scrolls_into_history_and_keeps_extending() {
    // The reachable half of edge scrolling: from the live bottom the only way
    // the view can move is UP into history, so this is the path a person
    // actually takes. (Dragging past the bottom does nothing at offset 0 —
    // there is nowhere further down to go.)
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    for i in 0..60 {
        feed(&mut rt, pane, format!("line{i}\r\n").as_bytes());
    }

    assert_eq!(
        rt.client_mut(client).expect("client").scroll_offset(pane),
        0,
        "starts at the live bottom, where a person starts"
    );

    // Press inside the pane, then drag above its top edge and hold there.
    let from = cell_at(&rt, client, pane, 0, 5);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    let anchor = selection(&mut rt, client, pane);
    assert_eq!(anchor, None, "a single-click press highlights nothing yet");

    let above = Point {
        x: from.x,
        y: origin(&rt, client, pane).y.saturating_sub(3),
    };
    rt.handle_mouse_input(client, drag_at(above), clock.tick());

    let held = selection(&mut rt, client, pane).expect("dragging out of the top highlights");
    let anchor_row = held.anchor.row;

    // Three timer firings scroll three lines into history, with no further
    // mouse event — the pointer is held still.
    for _ in 0..3 {
        rt.expire_selection_scrolls(clock.tick());
    }

    assert_eq!(
        rt.client_mut(client).expect("client").scroll_offset(pane),
        3,
        "the view walked three lines up into history, one per firing"
    );
    let after = selection(&mut rt, client, pane).expect("still highlighted");
    assert_eq!(
        after.anchor.row, anchor_row,
        "the anchor names the same line it always did — absolute rows do not \
         move when the view does"
    );
    assert_eq!(
        after.cursor.row,
        anchor_row - 8,
        "and the moving end reached three lines further back than the top row \
         it started on (5 rows up to the top, then 3 into history)"
    );
}

#[test]
fn two_clients_selecting_in_one_pane_never_see_each_others_highlight() {
    // The load-bearing per-client claim: highlights live on the Client, so two
    // terminals viewing the same pane select independently and neither sees the
    // other's. This is the axis that a per-pane-only model (zellij's) gets wrong.
    let (mut rt, alice, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    // A second client attached to the same session, viewing the same tab.
    let session_id = rt.session_for_client(alice).expect("alice's session").id;
    let tab = rt.client_mut(alice).expect("alice").active_tab();
    let bob = ClientId::new();
    let mut bob_client = koshi_session::client::Client::new(
        bob,
        session_id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 80, rows: 24 },
        tab,
    );
    bob_client.update_focused_pane(tab, pane);
    rt.sessions
        .get_mut(&session_id)
        .expect("session")
        .attach_client(bob_client);

    // Alice highlights.
    let from = cell_at(&rt, alice, pane, 0, 0);
    rt.handle_mouse_input(alice, press_at(from), clock.tick());
    rt.handle_mouse_input(
        alice,
        drag_at(cell_at(&rt, alice, pane, 4, 0)),
        clock.tick(),
    );

    assert!(
        selection(&mut rt, alice, pane).is_some(),
        "alice has a highlight"
    );
    assert_eq!(
        selection(&mut rt, bob, pane),
        None,
        "bob, on the same pane, has none"
    );
    // And it is invisible in bob's own frame, which is what he actually sees.
    assert!(
        rt.build_snapshot(bob).expect("bob's frame").panes[0]
            .selection
            .is_none(),
        "bob's frame draws no highlight"
    );
    assert!(
        rt.build_snapshot(alice).expect("alice's frame").panes[0]
            .selection
            .is_some(),
        "alice's frame draws hers"
    );
    // Alice's highlight holds only alice's view.
    assert!(rt.client_mut(alice).expect("alice").is_view_held(pane));
    assert!(
        !rt.client_mut(bob).expect("bob").is_view_held(pane),
        "bob's view of the same pane still follows live output"
    );
}

#[test]
fn a_drag_past_a_corner_scrolls_and_clamps_the_column() {
    // Past the top edge AND left of it at once: the vertical part scrolls, the
    // horizontal part just clamps — there is no more text sideways.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    for i in 0..60 {
        feed(&mut rt, pane, format!("line{i}\r\n").as_bytes());
    }

    let from = cell_at(&rt, client, pane, 10, 5);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    let corner = Point {
        x: origin(&rt, client, pane).x.saturating_sub(5),
        y: origin(&rt, client, pane).y.saturating_sub(5),
    };
    rt.handle_mouse_input(client, drag_at(corner), clock.tick());

    let selection = selection(&mut rt, client, pane).expect("a highlight");
    assert_eq!(selection.cursor.col, 0, "clamped to the first column");
    assert!(
        rt.client_mut(client)
            .expect("client")
            .selection_drag()
            .expect("a drag")
            .scroll_at
            .is_some(),
        "and the vertical overshoot still arms the scroll"
    );
}

#[test]
fn erasing_all_history_under_a_highlight_drops_it_and_frees_the_view() {
    // A highlight whose every line has been erased (`CSI 3 J`) can never draw
    // again, but it would still hold the view against live output with nothing
    // on screen to explain why. It must go.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 6, 0); // the `w`
    let to = cell_at(&rt, client, pane, 10, 0); // the `d`
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(client, drag_at(to), clock.tick());
    rt.handle_mouse_input(client, release_at(to), clock.tick());
    assert!(selection(&mut rt, client, pane).is_some(), "highlighted");

    // Sixty lines of output push `hello world` into history; the held view's
    // offset rises with it.
    for i in 0..60 {
        feed(&mut rt, pane, format!("line{i}\r\n").as_bytes());
    }
    assert!(
        selection(&mut rt, client, pane).is_some(),
        "output alone never clears a highlight"
    );

    // The child erases its scrollback. Every line under the highlight is gone.
    feed(&mut rt, pane, b"\x1b[3J");
    assert_eq!(
        selection(&mut rt, client, pane),
        None,
        "a highlight with nothing left to name is dropped"
    );
    assert!(
        !rt.client_mut(client).expect("client").is_view_held(pane),
        "and the view follows live output again"
    );
}

#[test]
fn a_highlight_still_partly_on_screen_survives_a_history_erase() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    for i in 0..60 {
        feed(&mut rt, pane, format!("line{i}\r\n").as_bytes());
    }

    // Scrolled up three lines, the top three view rows are history rows; a drag
    // from the top row down onto the live screen spans the boundary.
    rt.scroll_up(client, pane, 3);
    let from = cell_at(&rt, client, pane, 0, 0);
    let to = cell_at(&rt, client, pane, 4, 10);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(client, drag_at(to), clock.tick());
    rt.handle_mouse_input(client, release_at(to), clock.tick());
    let before = selection(&mut rt, client, pane).expect("a highlight");

    feed(&mut rt, pane, b"\x1b[3J");
    assert_eq!(
        selection(&mut rt, client, pane),
        Some(before),
        "a highlight with lines still on the live screen keeps them"
    );
}

#[test]
fn a_press_on_the_right_half_of_a_wide_glyph_names_the_glyph_itself() {
    // The pointer can land on the blank right half of a wide (CJK) glyph, a
    // width-0 cell the renderer never paints. The position must name the
    // glyph's own cell, or a highlight could cover only invisible cells.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, "世界x".as_bytes());

    // 世 covers columns 0-1, 界 columns 2-3, x column 4. Press on 世's blank
    // half, drag onto 界's blank half.
    let from = cell_at(&rt, client, pane, 1, 0);
    let to = cell_at(&rt, client, pane, 3, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(client, drag_at(to), clock.tick());

    let selection = selection(&mut rt, client, pane).expect("a highlight");
    assert_eq!(
        selection.anchor,
        GridPos { row: 0, col: 0 },
        "the anchor is 世's own cell, not its blank half"
    );
    assert_eq!(
        selection.cursor,
        GridPos { row: 0, col: 2 },
        "the cursor is 界's own cell, not its blank half"
    );
}

#[test]
fn a_held_drag_stops_firing_once_there_is_nowhere_left_to_scroll() {
    // At the live bottom a drag held below the pane has nothing to scroll
    // toward. The timer must stop rather than fire every 15ms doing nothing;
    // the next drag event re-arms it.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    let below = Point {
        x: from.x,
        y: origin(&rt, client, pane).y + 40,
    };
    rt.handle_mouse_input(client, drag_at(below), clock.tick());
    let now = clock.tick();
    assert!(
        rt.next_selection_scroll_wakeup(now).is_some(),
        "the overshoot arms the scroll"
    );

    // The firing finds the view already at the live bottom and moves nothing.
    rt.expire_selection_scrolls(now + Duration::from_millis(15));
    assert_eq!(
        rt.next_selection_scroll_wakeup(now + Duration::from_millis(15)),
        None,
        "a firing that moved nothing disarms the timer"
    );

    // The pointer moving again — still below the pane — re-arms it.
    rt.handle_mouse_input(client, drag_at(below), clock.tick());
    assert!(
        rt.next_selection_scroll_wakeup(clock.tick()).is_some(),
        "the next drag event arms it again"
    );
}

#[test]
fn a_held_drag_stops_firing_at_the_oldest_retained_line() {
    // The top-edge mirror: at the oldest line nothing more will ever appear
    // above, so a firing that moved nothing must not re-arm.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    for i in 0..60 {
        feed(&mut rt, pane, format!("line{i}\r\n").as_bytes());
    }
    rt.scroll_to_top(client, pane);

    let from = cell_at(&rt, client, pane, 0, 5);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    let above = Point {
        x: from.x,
        y: origin(&rt, client, pane).y.saturating_sub(3),
    };
    rt.handle_mouse_input(client, drag_at(above), clock.tick());
    let now = clock.tick();
    assert!(
        rt.next_selection_scroll_wakeup(now).is_some(),
        "the overshoot arms the scroll"
    );

    rt.expire_selection_scrolls(now + Duration::from_millis(15));
    assert_eq!(
        rt.next_selection_scroll_wakeup(now + Duration::from_millis(15)),
        None,
        "already at the oldest line, so the firing disarms the timer"
    );
}

#[test]
fn a_highlight_on_the_alternate_screen_survives_the_app_scrolling() {
    // Same ruling on the alternate screen: an app scrolling its own rows
    // (claude streaming, a build log) leaves the highlight where it was, even
    // if different text now sits under it. Any key into the pane clears it
    // (the exit rule), so keyboard-driven scrolling never even reaches this
    // state.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"\x1b[?1049h"); // enter the alternate screen
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    rt.handle_mouse_input(
        client,
        release_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    let highlighted = selection(&mut rt, client, pane).expect("highlighted");

    // The app scrolls on its own: cursor to the last row, a line feed there.
    feed(&mut rt, pane, b"\x1b[999;1H\n");
    assert_eq!(
        selection(&mut rt, client, pane),
        Some(highlighted),
        "the highlight stands until input into the pane clears it"
    );
}

#[test]
fn a_screen_highlight_survives_the_app_moving_rows_around() {
    // The app deleting or inserting lines moves screen rows, possibly leaving
    // the highlight over different text. Koshi leaves it alone — the app moved
    // the text, not koshi, and zellij behaves the same. The next click or key
    // into the pane clears it anyway, and copy captures the text at the drag's
    // release, so a moved highlight never corrupts a copy.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    rt.handle_mouse_input(
        client,
        release_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    let highlighted = selection(&mut rt, client, pane).expect("highlighted");

    // DL with the cursor on row 3: rows below slide up, nothing is pushed.
    feed(&mut rt, pane, b"\x1b[3;1H\x1b[M");
    assert_eq!(
        selection(&mut rt, client, pane),
        Some(highlighted),
        "the highlight stands; what the app did to its rows is its business"
    );
}

#[test]
fn a_history_highlight_survives_a_primary_row_shift() {
    // History rows do not move when screen rows do — their numbers still name
    // the same text — so a highlight living entirely in history stands.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    for i in 0..60 {
        feed(&mut rt, pane, format!("line{i}\r\n").as_bytes());
    }

    // Scrolled up three lines, the top three view rows are history rows.
    rt.scroll_up(client, pane, 3);
    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 1)),
        clock.tick(),
    );
    rt.handle_mouse_input(
        client,
        release_at(cell_at(&rt, client, pane, 4, 1)),
        clock.tick(),
    );
    let highlighted = selection(&mut rt, client, pane).expect("highlighted");

    feed(&mut rt, pane, b"\x1b[10;1H\x1b[M");
    assert_eq!(
        selection(&mut rt, client, pane),
        Some(highlighted),
        "a highlight entirely in history is untouched by screen row moves"
    );
}

#[test]
fn a_highlight_on_the_alternate_screen_survives_a_redraw_in_place() {
    // Rewriting cells without moving rows — how a full-screen app updates a
    // status line — leaves the highlight standing, exactly as an in-place
    // redraw does on the primary screen.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"\x1b[?1049h");
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    rt.handle_mouse_input(
        client,
        release_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    let highlighted = selection(&mut rt, client, pane).expect("highlighted");

    feed(&mut rt, pane, b"\x1b[2;1Hredrawn text");
    assert_eq!(
        selection(&mut rt, client, pane),
        Some(highlighted),
        "no rows moved, so the highlight stands"
    );
}

#[test]
fn a_plain_click_copies_nothing() {
    // A click's press highlights nothing, so its release has nothing to copy:
    // no clipboard write, and the clipboard the user already had is untouched.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let at = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(at), clock.tick());
    rt.handle_mouse_input(client, release_at(at), clock.tick());
    assert_eq!(rt.take_host_writes(client), None);
}

#[test]
fn releasing_the_gesture_is_the_copy() {
    // No copy key exists: like zellij, releasing the selection IS the copy.
    // The highlighted text goes to the client's outer terminal as OSC 52 —
    // which sets the OS clipboard — and the highlight stays standing.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    assert_eq!(
        rt.take_host_writes(client),
        None,
        "nothing is copied while the drag is still moving"
    );
    rt.handle_mouse_input(
        client,
        release_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );

    // base64("hello") = aGVsbG8=
    assert_eq!(
        rt.take_host_writes(client).expect("queued clipboard write"),
        b"\x1b]52;c;aGVsbG8=\x07".to_vec()
    );
    assert!(
        selection(&mut rt, client, pane).is_some(),
        "the highlight stays; the exit rules end it as usual"
    );
}

#[test]
fn internal_copy_on_select_switch_can_hold_the_copy_for_a_future_action() {
    let (mut rt, client, pane) = runtime();
    rt.config.copy.copy_on_select = false;
    feed(&mut rt, pane, b"hello world");

    select_hello(&mut rt, client, pane);

    assert_eq!(rt.take_host_writes(client), None);
    assert!(selection(&mut rt, client, pane).is_some());
}

#[cfg(not(feature = "native"))]
#[test]
fn native_target_falls_back_to_osc52_without_native_support() {
    let (mut rt, client, pane) = runtime();
    rt.config.copy.clipboard = ClipboardBackend::Native;
    feed(&mut rt, pane, b"hello world");

    select_hello(&mut rt, client, pane);

    assert_eq!(
        rt.take_host_writes(client).expect("OSC 52 fallback"),
        b"\x1b]52;c;aGVsbG8=\x07".to_vec()
    );
}

#[cfg(feature = "native")]
#[test]
fn native_target_writes_only_to_the_native_clipboard() {
    let (mut rt, client, pane) = runtime();
    let writes = Arc::new(Mutex::new(Vec::new()));
    rt.native_clipboard = Some(Box::new(RecordingClipboard {
        writes: Arc::clone(&writes),
    }));
    rt.config.copy.clipboard = ClipboardBackend::Native;
    feed(&mut rt, pane, b"hello world");

    select_hello(&mut rt, client, pane);

    assert_eq!(rt.take_host_writes(client), None);
    assert_eq!(*writes.lock().expect("recording lock"), vec!["hello"]);
}

#[cfg(feature = "native")]
#[test]
fn osc52_target_leaves_the_native_clipboard_untouched() {
    let (mut rt, client, pane) = runtime();
    let writes = Arc::new(Mutex::new(Vec::new()));
    rt.native_clipboard = Some(Box::new(RecordingClipboard {
        writes: Arc::clone(&writes),
    }));
    rt.config.copy.clipboard = ClipboardBackend::Osc52;
    feed(&mut rt, pane, b"hello world");

    select_hello(&mut rt, client, pane);

    assert_eq!(
        rt.take_host_writes(client).expect("OSC 52 write"),
        b"\x1b]52;c;aGVsbG8=\x07".to_vec()
    );
    assert!(writes.lock().expect("recording lock").is_empty());
}

#[cfg(feature = "native")]
#[test]
fn native_write_failure_leaves_koshi_running() {
    let (mut rt, client, pane) = runtime();
    rt.native_clipboard = Some(Box::new(FailingClipboard));
    rt.config.copy.clipboard = ClipboardBackend::Native;
    feed(&mut rt, pane, b"hello world");

    select_hello(&mut rt, client, pane);

    assert_eq!(rt.take_host_writes(client), None);
    assert!(selection(&mut rt, client, pane).is_some());
    assert!(rt.has_active_panes());
}

#[test]
fn both_target_always_writes_osc52() {
    let (mut rt, client, pane) = runtime();
    rt.config.copy.clipboard = ClipboardBackend::Both;
    feed(&mut rt, pane, b"hello world");

    #[cfg(feature = "native")]
    let writes = {
        let writes = Arc::new(Mutex::new(Vec::new()));
        rt.native_clipboard = Some(Box::new(RecordingClipboard {
            writes: Arc::clone(&writes),
        }));
        writes
    };

    select_hello(&mut rt, client, pane);

    assert_eq!(
        rt.take_host_writes(client).expect("OSC 52 write"),
        b"\x1b]52;c;aGVsbG8=\x07".to_vec()
    );
    #[cfg(feature = "native")]
    assert_eq!(*writes.lock().expect("recording lock"), vec!["hello"]);
}

#[test]
fn copy_release_obeys_disabled_trailing_whitespace_trimming() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    rt.config.copy.trim_trailing_whitespace = false;
    feed(&mut rt, pane, b"a");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, alt_press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 2, 0)),
        clock.tick(),
    );
    rt.handle_mouse_input(
        client,
        release_at(cell_at(&rt, client, pane, 2, 0)),
        clock.tick(),
    );

    assert_eq!(
        rt.take_host_writes(client).expect("queued clipboard write"),
        b"\x1b]52;c;YSAg\x07".to_vec()
    );
}

#[test]
fn ctrl_c_clears_the_highlight_like_any_key_reaching_the_pane() {
    // The exact chord a person presses to "copy": Ctrl+C. It is not bound, so
    // it falls through to the shell (SIGINT) — input reaching the pane's
    // child — and the highlight clears, exactly the behavior zellij shows.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    rt.handle_mouse_input(
        client,
        release_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    assert!(selection(&mut rt, client, pane).is_some(), "highlighted");

    rt.handle_key_input(
        client,
        KeyChord::new(ModFlags::CTRL, Key::Char('c')),
        clock.tick(),
    );
    assert_eq!(
        selection(&mut rt, client, pane),
        None,
        "Ctrl+C reached the shell, so the highlight is gone"
    );
}

#[test]
fn typing_into_the_pane_clears_the_typists_highlight_there() {
    // The exit rule: input reaching the pane's child leaves visual mode. A key
    // no binding consumes is written to the child, so it clears the highlight,
    // the way typing replaces a selection in an editor.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    rt.handle_mouse_input(
        client,
        release_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    assert!(selection(&mut rt, client, pane).is_some(), "highlighted");

    rt.handle_key_input(
        client,
        KeyChord::new(ModFlags::NONE, Key::Char('x')),
        clock.tick(),
    );
    assert_eq!(
        selection(&mut rt, client, pane),
        None,
        "the key reached the child, so the highlight is gone"
    );
}

#[test]
fn typing_during_a_drag_cancels_the_highlight_and_the_gesture() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    assert!(selection(&mut rt, client, pane).is_some());
    assert!(rt
        .client_mut(client)
        .expect("client")
        .selection_drag()
        .is_some());

    rt.handle_key_input(
        client,
        KeyChord::new(ModFlags::NONE, Key::Char('x')),
        clock.tick(),
    );
    assert_eq!(selection(&mut rt, client, pane), None);
    assert_eq!(
        rt.client_mut(client).expect("client").selection_drag(),
        None
    );

    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 6, 0)),
        clock.tick(),
    );
    assert_eq!(selection(&mut rt, client, pane), None);
}

#[test]
fn typing_after_a_press_cancels_the_empty_gesture() {
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    assert_eq!(selection(&mut rt, client, pane), None);
    assert!(rt
        .client_mut(client)
        .expect("client")
        .selection_drag()
        .is_some());

    rt.handle_key_input(
        client,
        KeyChord::new(ModFlags::NONE, Key::Char('x')),
        clock.tick(),
    );
    assert_eq!(
        rt.client_mut(client).expect("client").selection_drag(),
        None
    );
}

#[test]
fn typing_leaves_another_panes_highlight_and_drag_alone() {
    // Only the pane the key reaches ends selection activity; another pane's
    // highlight and in-flight drag are not this key's business.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    let highlighted = selection(&mut rt, client, pane).expect("highlighted");
    let drag = rt
        .client_mut(client)
        .expect("client")
        .selection_drag()
        .expect("dragging");

    // The split focuses the new pane, so the key types into it.
    let other = split(&mut rt, client);
    assert_ne!(other, pane);
    rt.handle_key_input(
        client,
        KeyChord::new(ModFlags::NONE, Key::Char('x')),
        clock.tick(),
    );
    assert_eq!(
        selection(&mut rt, client, pane),
        Some(highlighted),
        "the highlight in the unfocused pane stands"
    );
    assert_eq!(
        rt.client_mut(client).expect("client").selection_drag(),
        Some(drag),
        "the drag in the unfocused pane stands"
    );
}

#[test]
fn a_click_forwarded_to_a_mouse_aware_program_clears_the_highlight() {
    // Same exit rule for the mouse: a click the program asked to see reaches
    // the child, so the highlight gets out of its way.
    let (mut rt, client, pane) = runtime();
    let mut clock = Clock::new();
    feed(&mut rt, pane, b"hello world");

    let from = cell_at(&rt, client, pane, 0, 0);
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    rt.handle_mouse_input(
        client,
        drag_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    rt.handle_mouse_input(
        client,
        release_at(cell_at(&rt, client, pane, 4, 0)),
        clock.tick(),
    );
    assert!(selection(&mut rt, client, pane).is_some(), "highlighted");

    // The program turns mouse reporting on; the next press is its, not koshi's.
    feed(&mut rt, pane, b"\x1b[?1000h");
    rt.handle_mouse_input(client, press_at(from), clock.tick());
    assert_eq!(
        selection(&mut rt, client, pane),
        None,
        "the forwarded click reached the child, so the highlight is gone"
    );
}
