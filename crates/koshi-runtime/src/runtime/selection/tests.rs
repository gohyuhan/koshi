//! Tests for selecting text with the mouse, driven through both halves: the
//! viewer resolves each gesture against the frame it painted and the session
//! stores, snaps, and copies what came back.

use super::*;

use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};

use crate::runtime::pty_inbox::InboxSink;
use crate::runtime::tests::{
    apply_mouse_actions, build_mouse_frame, build_viewer, dispatch_mouse_input_at_time,
};
use koshi_client::Client as ViewerClient;
use koshi_config::layer::{PartialCopyConfig, PartialKoshiConfig};
use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, CopyArgs, GridPosition, NewPaneArgs,
    NewTabArgs, Selection, SelectionKind, SetSelectionArgs, VisualCommand,
};
use koshi_core::event::{Event, SelectionChanged};
use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::{CommandId, SessionId};
use koshi_core::key::{BindingModifierFlags, Key, KeyChord};
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind};
use koshi_renderer::snapshot::ViewerChrome;
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::fixtures::build_key_input_for_chord;

/// A runtime with one bootstrapped 80x24 client, its viewer half, and its
/// single pane.
fn build_selection_runtime() -> (Server, ViewerClient, PaneId) {
    let (event_sender, event_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(event_sender),
    )));
    let mut server = Server::from_runtime_parts(fake_pty_backend, event_receiver);
    let client_id = server
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let pane_id = *server.live_pane_ids.iter().next().expect("one pane");
    let viewer = build_viewer(&mut server, client_id);
    (server, viewer, pane_id)
}

/// The viewer from [`build_viewer`], with its `copy` settings overridden by
/// `copy_config`.
fn build_copying_viewer(
    server: &mut Server,
    client_id: ClientId,
    copy_config: PartialCopyConfig,
) -> ViewerClient {
    let mut viewer = build_viewer(server, client_id);
    viewer.load_startup_config(
        Some(PartialKoshiConfig {
            copy: Some(copy_config),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );
    viewer
}

/// Fire the selection drag's scroll timer at `current_time`, as the binary's
/// loop does.
fn expire_mouse_scroll(server: &mut Server, viewer: &mut ViewerClient, current_time: Instant) {
    let mouse_frame = build_mouse_frame(
        server
            .build_snapshot(viewer.get_client_id())
            .expect("render snapshot"),
    );
    let mouse_actions = viewer.expire_mouse_scroll(current_time, &mouse_frame);
    apply_mouse_actions(server, viewer, &mouse_frame, mouse_actions);
}

/// The screen rect of `pane_id`'s content area in `client_id`'s frame.
fn find_pane_content_rect(server: &Server, client_id: ClientId, pane_id: PaneId) -> Rect {
    let render_snapshot = server.build_snapshot(client_id).expect("render snapshot");
    koshi_renderer::find_pane_content_rect(
        render_snapshot.build_frame_layout(ViewerChrome::default()),
        pane_id,
    )
    .expect("content rect")
}

/// The screen cell for `pane_id`'s content row `row_index`, column
/// `column_index`.
fn get_pane_screen_cell(
    server: &Server,
    client_id: ClientId,
    pane_id: PaneId,
    column_index: u16,
    row_index: u16,
) -> Point {
    let content_origin = find_pane_content_rect(server, client_id, pane_id).origin;
    Point {
        column: content_origin.column + column_index,
        row: content_origin.row + row_index,
    }
}

/// `pane_id`'s last content column. The pane's border ring takes cells from
/// the client's viewport: a pane in an 80-column terminal is narrower than 80.
fn get_last_pane_content_column_index(
    server: &Server,
    client_id: ClientId,
    pane_id: PaneId,
) -> u16 {
    find_pane_content_rect(server, client_id, pane_id)
        .size
        .column_count
        - 1
}

fn build_mouse_press(position: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Press(MouseButton::Left),
        position,
        modifier_flags: BindingModifierFlags::NONE,
    }
}

fn build_alt_mouse_press(position: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Press(MouseButton::Left),
        position,
        modifier_flags: BindingModifierFlags::ALT,
    }
}

fn build_mouse_drag(position: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Drag(MouseButton::Left),
        position,
        modifier_flags: BindingModifierFlags::NONE,
    }
}

fn build_mouse_release(position: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Release(MouseButton::Left),
        position,
        modifier_flags: BindingModifierFlags::NONE,
    }
}

/// Turn on this client's mouse-select mode the way its keybinding does. A drag
/// then grabs the mouse for a koshi selection even over a program that asked
/// for the mouse. The viewer picks the change up from its subscription on the
/// next event.
fn enable_mouse_selection(server: &mut Server, client_id: ClientId) {
    let _ = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        Command::ToggleMouseSelect,
    ));
}

/// A test clock that starts at `Instant::now()` and moves only when a test
/// advances it.
struct Clock(Instant);

impl Clock {
    fn new() -> Self {
        Clock(Instant::now())
    }

    /// A second on from the last reading: two presses this far apart are two
    /// single clicks.
    fn advance_one_second(&mut self) -> Instant {
        self.0 += Duration::from_secs(1);
        self.0
    }

    /// A tenth of a second on: inside the 400ms threshold. A second press here
    /// is a double click.
    fn advance_double_click_interval(&mut self) -> Instant {
        self.0 += Duration::from_millis(100);
        self.0
    }

    /// The last reading, without moving the clock on.
    fn get_current_time(&self) -> Instant {
        self.0
    }
}

/// `client_id`'s highlight in `pane_id`.
fn get_selection(server: &mut Server, client_id: ClientId, pane_id: PaneId) -> Option<Selection> {
    server
        .get_client_mut(client_id)
        .expect("client")
        .get_selection(pane_id)
}

/// Split the focused pane rightward and return the new pane's id.
fn split_pane_rightward(server: &mut Server, client_id: ClientId) -> PaneId {
    let existing_pane_ids: Vec<PaneId> = server.live_pane_ids.iter().copied().collect();
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        Command::NewPane(NewPaneArgs {
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Right,
            should_stack: false,
            working_directory: None,
            spawn_spec: None,
            client_id: None,
        }),
    );
    let _ = server.dispatch(command_envelope);
    *server
        .live_pane_ids
        .iter()
        .find(|pane_id| !existing_pane_ids.contains(pane_id))
        .expect("a new pane")
}

#[test]
fn a_drag_highlights_from_the_press_to_the_pointer() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 6, 0); // the `w`
    let end_point = get_pane_screen_cell(&server, client_id, pane_id, 10, 0); // the `d`
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );

    let selection = get_selection(&mut server, client_id, pane_id).expect("a highlight");
    assert_eq!(selection.selection_kind, SelectionKind::Character);
    assert_eq!(
        selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 6
        }
    );
    assert_eq!(
        selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 10
        }
    );
}

#[test]
fn a_press_with_no_drag_leaves_no_highlight() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    // A plain click: press and release, the pointer never moving. Nothing is
    // highlighted, and in particular no empty highlight is left to hold the view.
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 3, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );

    assert_eq!(get_selection(&mut server, client_id, pane_id), None);
    assert!(
        !server
            .get_client_mut(client_id)
            .expect("client")
            .is_view_held(pane_id),
        "a click leaves the view following live output"
    );
}

#[test]
fn a_press_drops_the_highlight_that_was_up() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    let highlighted_selection =
        get_selection(&mut server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted_selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted_selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );

    // Pressing again clears it.
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    assert_eq!(get_selection(&mut server, client_id, pane_id), None);
}

#[test]
fn a_drag_in_one_pane_leaves_the_other_panes_highlight_alone() {
    let (mut server, mut viewer, initial_pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    let split_pane_id = split_pane_rightward(&mut server, client_id);
    server.handle_pty_output(initial_pane_id, b"first pane");
    server.handle_pty_output(split_pane_id, b"second pane");

    // Highlight in the second pane (the split focused it).
    let start_point = get_pane_screen_cell(&server, client_id, split_pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&server, client_id, split_pane_id, 5, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    let split_pane_selection =
        get_selection(&mut server, client_id, split_pane_id).expect("second pane highlighted");

    // Now focus the first pane and highlight there. A click on an unfocused pane
    // only focuses, so it takes a second press to start the drag.
    let initial_pane_start_point = get_pane_screen_cell(&server, client_id, initial_pane_id, 0, 0);
    let initial_pane_end_point = get_pane_screen_cell(&server, client_id, initial_pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(initial_pane_start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(initial_pane_start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(initial_pane_end_point),
        clock.advance_one_second(),
    );

    let initial_pane_selection = get_selection(&mut server, client_id, initial_pane_id)
        .expect("the first pane is highlighted");
    assert_eq!(
        initial_pane_selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        initial_pane_selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );
    assert_eq!(
        get_selection(&mut server, client_id, split_pane_id),
        Some(split_pane_selection),
        "the second pane's highlight is exactly as it was"
    );
}

#[test]
fn a_focus_click_on_another_pane_clears_no_highlight() {
    let (mut server, mut viewer, initial_pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    let split_pane_id = split_pane_rightward(&mut server, client_id);
    server.handle_pty_output(initial_pane_id, b"first pane");
    server.handle_pty_output(split_pane_id, b"second pane");

    let start_point = get_pane_screen_cell(&server, client_id, split_pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&server, client_id, split_pane_id, 5, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    let split_pane_selection =
        get_selection(&mut server, client_id, split_pane_id).expect("second pane highlighted");

    // One click on the other pane is a koshi focus trigger. It never reaches
    // the highlighted pane's program and clears nothing.
    let screen_point = get_pane_screen_cell(&server, client_id, initial_pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );

    assert_eq!(
        get_selection(&mut server, client_id, split_pane_id),
        Some(split_pane_selection),
        "focusing away leaves the highlight up"
    );
}

#[test]
fn mouse_select_mode_selects_over_a_mouse_aware_program() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");
    // The program turns on mouse tracking: bare gestures are now its own.
    server.handle_pty_output(pane_id, b"\x1b[?1000h\x1b[?1006h");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);

    // With mouse-select off, the drag is forwarded to the program: no highlight.
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(end_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        None,
        "with mouse-select off, a drag over a mouse-aware program does not select"
    );

    // Turn mouse-select on: the same gesture now highlights in koshi.
    enable_mouse_selection(&mut server, client_id);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(end_point),
        clock.advance_one_second(),
    );
    let selection =
        get_selection(&mut server, client_id, pane_id).expect("mouse-select drag highlights");
    assert_eq!(selection.selection_kind, SelectionKind::Character);
    assert_eq!(
        selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );
}

#[test]
fn mouse_select_mode_still_focuses_an_unfocused_pane_first() {
    let (mut server, mut viewer, initial_pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    let split_pane_id = split_pane_rightward(&mut server, client_id); // the split focuses `split_pane_id`
    server.handle_pty_output(initial_pane_id, b"first pane");
    server.handle_pty_output(split_pane_id, b"second pane");
    enable_mouse_selection(&mut server, client_id);

    let start_point = get_pane_screen_cell(&server, client_id, initial_pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&server, client_id, initial_pane_id, 4, 0);

    // The focus-first rule runs before mouse-select is consulted: the first
    // press on the unfocused pane only focuses it.
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        get_selection(&mut server, client_id, initial_pane_id),
        None,
        "the focusing press does not also select"
    );

    // Now focused, a second press then drag highlights.
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    let highlighted_selection = get_selection(&mut server, client_id, initial_pane_id)
        .expect("the second drag selects in the now-focused pane");
    assert_eq!(
        highlighted_selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted_selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );
}

#[test]
fn a_double_click_drag_snaps_to_whole_words() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    // Two presses inside the threshold, then a drag: word selection.
    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 2, 0); // inside `hello`
    let end_point = get_pane_screen_cell(&server, client_id, pane_id, 8, 0); // inside `world`
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_double_click_interval(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_double_click_interval(),
    );

    let selection = get_selection(&mut server, client_id, pane_id).expect("a highlight");
    assert_eq!(selection.selection_kind, SelectionKind::Word);
    assert_eq!(
        selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        },
        "the anchor fell back to the start of `hello`"
    );
    assert_eq!(
        selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 10
        },
        "and the cursor ran on to the end of `world`"
    );
}

#[test]
fn a_triple_click_drag_snaps_to_whole_lines() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_double_click_interval(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_double_click_interval(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_double_click_interval(),
    );

    let last_column_index = get_last_pane_content_column_index(&server, client_id, pane_id);
    let selection = get_selection(&mut server, client_id, pane_id).expect("a highlight");
    assert_eq!(selection.selection_kind, SelectionKind::Line);
    assert_eq!(
        selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: last_column_index
        },
        "a line selection runs to the last column"
    );
}

#[test]
fn a_fourth_quick_click_starts_the_run_over() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    for _ in 0..4 {
        dispatch_mouse_input_at_time(
            &mut server,
            &mut viewer,
            build_mouse_press(screen_point),
            clock.advance_double_click_interval(),
        );
    }
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 6, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_double_click_interval(),
    );

    assert_eq!(
        get_selection(&mut server, client_id, pane_id)
            .expect("a highlight")
            .selection_kind,
        SelectionKind::Character,
        "the run wraps back to a single click after three"
    );
}

#[test]
fn two_slow_clicks_are_two_single_clicks() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    // A second apart is well past the 400ms threshold. This is not a double
    // click, and the drag selects characters, not the whole word.
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 2, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 3, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );

    assert_eq!(
        get_selection(&mut server, client_id, pane_id)
            .expect("a highlight")
            .selection_kind,
        SelectionKind::Character
    );
}

#[test]
fn alt_held_at_the_press_makes_a_block() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"one\r\ntwo\r\nthree");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 1, 0);
    let end_point = get_pane_screen_cell(&server, client_id, pane_id, 2, 2);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_alt_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );

    let selection = get_selection(&mut server, client_id, pane_id).expect("a highlight");
    assert_eq!(selection.selection_kind, SelectionKind::Block);
    assert_eq!(
        selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 1
        }
    );
    assert_eq!(
        selection.cursor,
        GridPosition {
            row_index: 2,
            column_index: 2
        }
    );
}

#[test]
fn alt_wins_over_a_double_click() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    // Alt held at the second press makes a block, even inside the double-click
    // threshold.
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 2, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_alt_mouse_press(screen_point),
        clock.advance_double_click_interval(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_double_click_interval(),
    );

    assert_eq!(
        get_selection(&mut server, client_id, pane_id)
            .expect("a highlight")
            .selection_kind,
        SelectionKind::Block
    );
}

#[test]
fn a_release_ends_the_drag_but_leaves_the_highlight() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(end_point),
        clock.advance_one_second(),
    );

    let selection_after_release =
        get_selection(&mut server, client_id, pane_id).expect("the highlight is retained");

    // A later drag with no press behind it extends nothing.
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 8, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        Some(selection_after_release),
        "a drag with no press behind it changes nothing"
    );
}

#[test]
fn a_highlight_holds_the_view_at_the_live_bottom() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );

    assert!(
        server
            .get_client_mut(client_id)
            .expect("client")
            .is_view_held(pane_id),
        "a highlight holds the view even at offset 0, which an offset alone \
         cannot express"
    );
}

#[test]
fn a_drag_past_the_bottom_edge_scrolls_and_keeps_extending() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    // Fill well past the 24-row screen. The extra lines go to history.
    for line_number in 0..60 {
        server.handle_pty_output(pane_id, format!("line{line_number}\r\n").as_bytes());
    }

    // Scroll up 10 lines, then drag past the bottom edge.
    server.scroll_up(client_id, pane_id, 10);
    let scroll_offset_before_timer = server
        .get_client_mut(client_id)
        .expect("client")
        .get_scroll_offset(pane_id);
    assert_eq!(scroll_offset_before_timer, 10);

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let below_pane_point = Point {
        column: start_point.column,
        row: find_pane_content_rect(&server, client_id, pane_id)
            .origin
            .row
            + 100,
    };
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(below_pane_point),
        clock.advance_one_second(),
    );
    let selection_during_drag =
        get_selection(&mut server, client_id, pane_id).expect("the drag highlights");

    // The drag arms the scroll timer and does not scroll on the event itself.
    assert_eq!(
        viewer.compute_next_mouse_wakeup(clock.get_current_time()),
        Some(Duration::from_millis(15)),
        "a pointer past the bottom edge arms the scroll timer"
    );

    // Firing the timer scrolls one line toward live output and keeps the
    // highlight extending, without any further mouse event.
    expire_mouse_scroll(&mut server, &mut viewer, clock.advance_one_second());
    assert_eq!(
        server
            .get_client_mut(client_id)
            .expect("client")
            .get_scroll_offset(pane_id),
        scroll_offset_before_timer - 1,
        "one line per firing, toward the pointer"
    );
    let selection_after_timer =
        get_selection(&mut server, client_id, pane_id).expect("and the highlight keeps up");
    assert_eq!(
        selection_after_timer.anchor, selection_during_drag.anchor,
        "the anchor stays on the cell the press named"
    );
    assert_eq!(
        selection_after_timer.cursor.row_index,
        selection_during_drag.cursor.row_index + 1,
        "and the moving end followed the view one line down"
    );
}

#[test]
fn a_pointer_back_inside_the_pane_stops_the_scrolling() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    for line_number in 0..60 {
        server.handle_pty_output(pane_id, format!("line{line_number}\r\n").as_bytes());
    }
    server.scroll_up(client_id, pane_id, 10);

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let below_pane_point = Point {
        column: start_point.column,
        row: find_pane_content_rect(&server, client_id, pane_id)
            .origin
            .row
            + 100,
    };
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(below_pane_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        viewer.compute_next_mouse_wakeup(clock.get_current_time()),
        Some(Duration::from_millis(15)),
        "the overshoot arms the scroll"
    );

    // Back inside: the scroll disarms and the view stops moving on its own.
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 2);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        viewer.compute_next_mouse_wakeup(clock.get_current_time()),
        None,
        "a pointer inside the pane does not scroll"
    );

    let held_scroll_offset = server
        .get_client_mut(client_id)
        .expect("client")
        .get_scroll_offset(pane_id);
    expire_mouse_scroll(&mut server, &mut viewer, clock.advance_one_second());
    assert_eq!(
        server
            .get_client_mut(client_id)
            .expect("client")
            .get_scroll_offset(pane_id),
        held_scroll_offset,
        "a disarmed drag scrolls nothing when the timer runs"
    );
}

#[test]
fn a_wakeup_is_asked_for_only_while_a_drag_is_held_past_an_edge() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");
    let current_time = clock.advance_one_second();

    assert_eq!(
        viewer.compute_next_mouse_wakeup(current_time),
        None,
        "an idle client asks for no wakeup"
    );

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        viewer.compute_next_mouse_wakeup(clock.advance_one_second()),
        None,
        "a drag inside the pane asks for no wakeup"
    );

    let below_pane_point = Point {
        column: start_point.column,
        row: find_pane_content_rect(&server, client_id, pane_id)
            .origin
            .row
            + 100,
    };
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(below_pane_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        viewer.compute_next_mouse_wakeup(clock.advance_one_second()),
        Some(Duration::ZERO),
        "a drag past the bottom edge asks the loop to wake, and one second later the wakeup is overdue"
    );
}

#[test]
fn switching_to_the_alternate_screen_drops_the_highlight() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    let highlighted_selection =
        get_selection(&mut server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted_selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted_selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );

    // The program enters the alternate screen (what `vim` does on start). A row
    // number counts lines pushed into scrollback. The alternate screen has no
    // scrollback, and the highlight names nothing there.
    server.handle_pty_output(pane_id, b"\x1b[?1049h");

    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        None,
        "the highlight went with the screen"
    );
    assert!(
        !server
            .get_client_mut(client_id)
            .expect("client")
            .is_view_held(pane_id),
        "and the view is no longer held by it"
    );
}

#[test]
fn a_drag_beyond_the_last_column_clamps_to_the_edge() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    // Far to the right of the pane, the highlight stops at the last column.
    let right_of_pane_point = Point {
        column: find_pane_content_rect(&server, client_id, pane_id)
            .origin
            .column
            + 200,
        row: start_point.row,
    };
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(right_of_pane_point),
        clock.advance_one_second(),
    );

    let last_column_index = get_last_pane_content_column_index(&server, client_id, pane_id);
    let selection = get_selection(&mut server, client_id, pane_id).expect("a highlight");
    assert_eq!(
        selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: last_column_index
        }
    );
    assert_eq!(
        viewer.compute_next_mouse_wakeup(clock.get_current_time()),
        None,
        "a sideways overshoot does not scroll"
    );
}

#[test]
fn a_drag_up_leaves_the_anchor_after_the_cursor() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"one\r\ntwo\r\nthree");

    // Press on the third row and drag up to the first. The anchor stays where
    // the press was and is the later end.
    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 2, 2);
    let end_point = get_pane_screen_cell(&server, client_id, pane_id, 1, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );

    let selection = get_selection(&mut server, client_id, pane_id).expect("a highlight");
    assert_eq!(
        selection.anchor,
        GridPosition {
            row_index: 2,
            column_index: 2
        }
    );
    assert_eq!(
        selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 1
        }
    );
}

#[test]
fn switching_tabs_ends_the_drag_and_keeps_the_highlight() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    let held_selection = get_selection(&mut server, client_id, pane_id).expect("highlighted");

    // A new tab takes the client's view. The drag's pane is no longer on the
    // frame.
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        Command::NewTab(NewTabArgs::default()),
    );
    let _ = server.dispatch(command_envelope);

    // A further drag extends nothing: the gesture went with the tab.
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        Some(held_selection),
        "the highlight belongs to its pane and is found again on switching back"
    );
}

#[test]
fn output_under_a_highlight_leaves_it_on_the_same_text_and_the_same_screen_row() {
    // The highlight is stored once, with absolute row numbers, and is never
    // re-anchored. Output arriving underneath moves neither the text it names
    // nor where it draws.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"target line\r\n");

    // Highlight `target` on the first row.
    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&server, client_id, pane_id, 5, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    let selection_before_output =
        get_selection(&mut server, client_id, pane_id).expect("a highlight");
    let drawn_row_spans_before_output = get_drawn_row_spans(&server, client_id);

    // Enough output to push the highlighted line off the top of the screen.
    for line_number in 0..30 {
        server.handle_pty_output(pane_id, format!("noise{line_number}\r\n").as_bytes());
    }

    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        Some(selection_before_output),
        "the stored highlight was never touched"
    );
    assert_eq!(
        get_drawn_row_spans(&server, client_id),
        drawn_row_spans_before_output,
        "and it still draws on the same screen row: the view is held and the \
         text under it does not move"
    );
}

/// The highlight rows the client's first pane draws this frame.
fn get_drawn_row_spans(server: &Server, client_id: ClientId) -> Option<Vec<(u16, u16, u16)>> {
    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    render_snapshot.pane_snapshots[0]
        .selection_spans
        .as_ref()
        .map(|selection_spans| selection_spans.row_spans.clone())
}

#[test]
fn a_word_on_the_alternate_screen_never_reaches_into_the_primarys_history() {
    // The alternate screen keeps no scrollback of its own. Growing a word from
    // its top row stops there and does not walk up into the lines the primary
    // screen pushed into history.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();

    // A single very long line on the primary screen wraps many times. Every
    // row it pushes into history ends in a soft wrap: the text continues onto
    // the row below.
    server.handle_pty_output(pane_id, "x".repeat(78 * 40).as_bytes());
    // Enter the alternate screen, which keeps the primary screen's history, and
    // write `ab foo bar` on it. The word `foo` starts at column 3.
    server.handle_pty_output(pane_id, b"\x1b[?47h");
    server.handle_pty_output(pane_id, b"ab foo bar");

    let primary_history_line_count = server
        .terminal_engine_by_pane_id
        .get(&pane_id)
        .expect("engine")
        .get_terminal_state()
        .get_scrollback()
        .get_total_pushed_line_count();
    assert!(
        primary_history_line_count > 0,
        "the primary pushed soft-wrapped rows into history"
    );

    // Double-click drag on `foo`.
    let word_press_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(word_press_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(word_press_point),
        clock.advance_double_click_interval(),
    );
    let word_drag_point = get_pane_screen_cell(&server, client_id, pane_id, 5, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(word_drag_point),
        clock.advance_double_click_interval(),
    );

    let selection = get_selection(&mut server, client_id, pane_id).expect("a highlight");
    assert_eq!(
        selection.anchor,
        GridPosition {
            row_index: primary_history_line_count,
            column_index: 3
        },
        "the word starts at the `f` of `foo` on the alternate screen"
    );
}

#[test]
fn a_plain_double_click_selects_the_word_under_the_pointer() {
    // The everyday gesture: double-click a word, no drag at all.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 8, 0); // inside `world`
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_double_click_interval(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_double_click_interval(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_double_click_interval(),
    );

    let selection = get_selection(&mut server, client_id, pane_id).expect("`world` is highlighted");
    assert_eq!(selection.selection_kind, SelectionKind::Word);
    assert_eq!(
        selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 6
        }
    );
    assert_eq!(
        selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 10
        }
    );
}

#[test]
fn a_plain_triple_click_selects_the_line_under_the_pointer() {
    // A triple click without a drag selects the whole line.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    for _ in 0..3 {
        dispatch_mouse_input_at_time(
            &mut server,
            &mut viewer,
            build_mouse_press(screen_point),
            clock.advance_double_click_interval(),
        );
        dispatch_mouse_input_at_time(
            &mut server,
            &mut viewer,
            build_mouse_release(screen_point),
            clock.advance_double_click_interval(),
        );
    }

    let last_column_index = get_last_pane_content_column_index(&server, client_id, pane_id);
    let selection =
        get_selection(&mut server, client_id, pane_id).expect("the line is highlighted");
    assert_eq!(selection.selection_kind, SelectionKind::Line);
    assert_eq!(
        selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: last_column_index
        }
    );
}

#[test]
fn a_double_click_then_a_drag_extends_from_the_same_word() {
    // The second press highlights the word. Dragging on extends by whole words
    // from the same anchor.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let word_press_point = get_pane_screen_cell(&server, client_id, pane_id, 2, 0); // inside `hello`
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(word_press_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(word_press_point),
        clock.advance_double_click_interval(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(word_press_point),
        clock.advance_double_click_interval(),
    );
    let selection_after_press =
        get_selection(&mut server, client_id, pane_id).expect("`hello` is highlighted");
    assert_eq!(
        selection_after_press.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        },
        "just `hello`"
    );

    let word_drag_point = get_pane_screen_cell(&server, client_id, pane_id, 8, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(word_drag_point),
        clock.advance_double_click_interval(),
    );
    let selection_after_drag =
        get_selection(&mut server, client_id, pane_id).expect("still highlighted");
    assert_eq!(
        selection_after_drag.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        },
        "same anchor"
    );
    assert_eq!(
        selection_after_drag.cursor,
        GridPosition {
            row_index: 0,
            column_index: 10
        },
        "extended to the end of `world`"
    );
}

#[test]
fn a_double_click_on_empty_space_leaves_no_view_held_over_nothing() {
    // A double click highlights at the press. On blank cells, the highlight
    // covers the blank run under the pointer and is never empty.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hi");

    // Column 40 is blank space well past the text.
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 40, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_double_click_interval(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_double_click_interval(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_double_click_interval(),
    );

    // Blanks are separators. A double click on a separator selects the run of
    // that character: every blank from the end of `hi` to the row's last
    // column.
    let selection =
        get_selection(&mut server, client_id, pane_id).expect("the blank run under the pointer");
    assert_eq!(
        selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 2
        },
        "the run starts right after `hi`"
    );
    assert_eq!(
        selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: get_last_pane_content_column_index(&server, client_id, pane_id),
        },
        "and reaches the row's last column"
    );
    assert!(
        server
            .get_client_mut(client_id)
            .expect("client")
            .is_view_held(pane_id),
        "a real highlight holds the view, and a click clears it again"
    );
}

#[test]
fn a_drag_past_the_top_edge_scrolls_into_history_and_keeps_extending() {
    // From the live bottom, a drag past the top edge scrolls up into history.
    // A drag past the bottom edge at scroll offset 0 scrolls nothing.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    for line_number in 0..60 {
        server.handle_pty_output(pane_id, format!("line{line_number}\r\n").as_bytes());
    }

    assert_eq!(
        server
            .get_client_mut(client_id)
            .expect("client")
            .get_scroll_offset(pane_id),
        0,
        "starts at the live bottom"
    );

    // Press inside the pane, then drag above its top edge and hold there.
    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 5);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        None,
        "a single-click press highlights nothing yet"
    );

    let above_pane_point = Point {
        column: start_point.column,
        row: find_pane_content_rect(&server, client_id, pane_id)
            .origin
            .row
            .saturating_sub(3),
    };
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(above_pane_point),
        clock.advance_one_second(),
    );

    let selection_after_drag =
        get_selection(&mut server, client_id, pane_id).expect("dragging out of the top highlights");
    let anchor_row_index = selection_after_drag.anchor.row_index;

    // Three timer firings scroll three lines into history. The pointer is held
    // still and sends no further mouse event.
    for _ in 0..3 {
        expire_mouse_scroll(&mut server, &mut viewer, clock.advance_one_second());
    }

    assert_eq!(
        server
            .get_client_mut(client_id)
            .expect("client")
            .get_scroll_offset(pane_id),
        3,
        "the view walked three lines up into history, one per firing"
    );
    let selection_after_scroll =
        get_selection(&mut server, client_id, pane_id).expect("still highlighted");
    assert_eq!(
        selection_after_scroll.anchor.row_index, anchor_row_index,
        "the anchor names the same line: absolute rows do not move when the view \
         does"
    );
    assert_eq!(
        selection_after_scroll.cursor.row_index,
        anchor_row_index - 8,
        "and the moving end is 8 rows above the anchor: 5 rows up to the top, \
         then 3 into history"
    );
}

#[test]
fn two_clients_selecting_in_one_pane_never_see_each_others_highlight() {
    // Highlights live on the Client. Two terminals viewing the same pane select
    // independently, and neither sees the other's highlight.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let first_client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    // A second client attached to the same session, viewing the same tab.
    let session_id = server
        .get_session_for_client(first_client_id)
        .expect("first client's session")
        .session_id;
    let tab_id = server
        .get_client_mut(first_client_id)
        .expect("first client")
        .get_active_tab_id();
    let second_client_id = ClientId::new();
    let mut second_client = koshi_session::client::Client::from_attachment(
        second_client_id,
        session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        tab_id,
        koshi_session::client::ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    second_client.update_focused_pane(tab_id, pane_id);
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("session")
        .attach_client(second_client);

    // The first client highlights.
    let start_point = get_pane_screen_cell(&server, first_client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, first_client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );

    let first_client_selection =
        get_selection(&mut server, first_client_id, pane_id).expect("first client has a highlight");
    assert_eq!(
        first_client_selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        first_client_selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );
    assert_eq!(
        get_selection(&mut server, second_client_id, pane_id),
        None,
        "second client, on the same pane, has none"
    );
    // The second client's own frame draws no highlight either.
    assert_eq!(
        server
            .build_snapshot(second_client_id)
            .expect("second client's frame")
            .pane_snapshots[0]
            .selection_spans,
        None,
        "second client's frame draws no highlight"
    );
    assert_eq!(
        server
            .build_snapshot(first_client_id)
            .expect("first client's frame")
            .pane_snapshots[0]
            .selection_spans
            .as_ref()
            .expect("first client's frame draws the highlight")
            .row_spans,
        vec![(0, 0, 4)],
        "one row, columns 0 to 4 inclusive"
    );
    // The first client's highlight holds only the first client's view.
    assert!(server
        .get_client_mut(first_client_id)
        .expect("first client")
        .is_view_held(pane_id));
    assert!(
        !server
            .get_client_mut(second_client_id)
            .expect("second client")
            .is_view_held(pane_id),
        "second client's view of the same pane still follows live output"
    );
}

#[test]
fn a_drag_past_a_corner_scrolls_and_clamps_the_column() {
    // Past the top edge and the left edge at once, the vertical part scrolls
    // and the horizontal part clamps to the first column.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    for line_number in 0..60 {
        server.handle_pty_output(pane_id, format!("line{line_number}\r\n").as_bytes());
    }

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 10, 5);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let pane_content_origin = find_pane_content_rect(&server, client_id, pane_id).origin;
    let above_left_corner_point = Point {
        column: pane_content_origin.column.saturating_sub(5),
        row: pane_content_origin.row.saturating_sub(5),
    };
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(above_left_corner_point),
        clock.advance_one_second(),
    );

    let selection = get_selection(&mut server, client_id, pane_id).expect("a highlight");
    assert_eq!(
        selection.cursor.column_index, 0,
        "clamped to the first column"
    );
    assert_eq!(
        viewer.compute_next_mouse_wakeup(clock.get_current_time()),
        Some(Duration::from_millis(15)),
        "and the vertical overshoot still arms the scroll"
    );
}

#[test]
fn erasing_all_history_under_a_highlight_drops_it_and_frees_the_view() {
    // Erasing history (`CSI 3 J`) under a highlight whose every line is in
    // history drops the highlight and releases the held view.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 6, 0); // the `w`
    let end_point = get_pane_screen_cell(&server, client_id, pane_id, 10, 0); // the `d`
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(end_point),
        clock.advance_one_second(),
    );
    let highlighted_selection =
        get_selection(&mut server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted_selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 6
        }
    );
    assert_eq!(
        highlighted_selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 10
        }
    );

    // Sixty lines of output push `hello world` into history; the held view's
    // offset rises with it.
    for line_number in 0..60 {
        server.handle_pty_output(pane_id, format!("line{line_number}\r\n").as_bytes());
    }
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        Some(highlighted_selection),
        "output alone never clears a highlight"
    );

    // The child erases its scrollback. Every line under the highlight is gone.
    server.handle_pty_output(pane_id, b"\x1b[3J");
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        None,
        "a highlight with nothing left to name is dropped"
    );
    assert!(
        !server
            .get_client_mut(client_id)
            .expect("client")
            .is_view_held(pane_id),
        "and the view follows live output again"
    );
}

#[test]
fn a_highlight_still_partly_on_screen_survives_a_history_erase() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    for line_number in 0..60 {
        server.handle_pty_output(pane_id, format!("line{line_number}\r\n").as_bytes());
    }

    // Scrolled up three lines, the top three view rows are history rows. A drag
    // from the top row down onto the live screen spans the boundary.
    server.scroll_up(client_id, pane_id, 3);
    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 10);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(end_point),
        clock.advance_one_second(),
    );
    let selection_before_history_erase =
        get_selection(&mut server, client_id, pane_id).expect("a highlight");

    server.handle_pty_output(pane_id, b"\x1b[3J");
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        Some(selection_before_history_erase),
        "a highlight with lines still on the live screen keeps them"
    );
}

#[test]
fn a_press_on_the_right_half_of_a_wide_glyph_names_the_glyph_itself() {
    // The pointer can land on the blank right half of a wide (CJK) glyph, a
    // width-0 cell the renderer never paints. The position names the glyph's
    // own cell.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, "世界x".as_bytes());

    // 世 covers columns 0-1, 界 columns 2-3, x column 4. Press on 世's blank
    // half, drag onto 界's blank half.
    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 1, 0);
    let end_point = get_pane_screen_cell(&server, client_id, pane_id, 3, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );

    let selection = get_selection(&mut server, client_id, pane_id).expect("a highlight");
    assert_eq!(
        selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        },
        "the anchor is 世's own cell, not its blank half"
    );
    assert_eq!(
        selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 2
        },
        "the cursor is 界's own cell, not its blank half"
    );
}

#[test]
fn a_held_drag_stops_firing_once_there_is_nowhere_left_to_scroll() {
    // At the live bottom, a drag held below the pane has nothing to scroll
    // toward. The first firing that moves nothing stops the timer. The next
    // drag event re-arms it.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let below_pane_point = Point {
        column: start_point.column,
        row: find_pane_content_rect(&server, client_id, pane_id)
            .origin
            .row
            + 40,
    };
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(below_pane_point),
        clock.advance_one_second(),
    );
    let current_time = clock.advance_one_second();
    assert_eq!(
        viewer.compute_next_mouse_wakeup(current_time),
        Some(Duration::ZERO),
        "the overshoot arms the scroll, and one second later the wakeup is overdue"
    );

    // The firing finds the view already at the live bottom and moves nothing.
    expire_mouse_scroll(
        &mut server,
        &mut viewer,
        current_time + Duration::from_millis(15),
    );
    assert_eq!(
        viewer.compute_next_mouse_wakeup(current_time + Duration::from_millis(15)),
        None,
        "a firing that moved nothing disarms the timer"
    );

    // The pointer moves again, still below the pane, and re-arms the timer.
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(below_pane_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        viewer.compute_next_mouse_wakeup(clock.advance_one_second()),
        Some(Duration::ZERO),
        "the next drag event arms it again"
    );
}

#[test]
fn a_held_drag_stops_firing_at_the_oldest_retained_line() {
    // At the oldest retained line, a drag held above the pane has nothing to
    // scroll toward. The first firing that moves nothing stops the timer.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    for line_number in 0..60 {
        server.handle_pty_output(pane_id, format!("line{line_number}\r\n").as_bytes());
    }
    server.scroll_up(client_id, pane_id, usize::MAX);

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 5);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let above_pane_point = Point {
        column: start_point.column,
        row: find_pane_content_rect(&server, client_id, pane_id)
            .origin
            .row
            .saturating_sub(3),
    };
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(above_pane_point),
        clock.advance_one_second(),
    );
    let current_time = clock.advance_one_second();
    assert_eq!(
        viewer.compute_next_mouse_wakeup(current_time),
        Some(Duration::ZERO),
        "the overshoot arms the scroll, and one second later the wakeup is overdue"
    );

    expire_mouse_scroll(
        &mut server,
        &mut viewer,
        current_time + Duration::from_millis(15),
    );
    assert_eq!(
        viewer.compute_next_mouse_wakeup(current_time + Duration::from_millis(15)),
        None,
        "the view is already at the oldest line, and the firing disarms the timer"
    );
}

#[test]
fn a_highlight_on_the_alternate_screen_survives_the_app_scrolling() {
    // On the alternate screen, an app scrolling its own rows (a streaming log,
    // a build log) leaves the highlight where it was, even if different text
    // now sits under it. Any key into the pane clears the highlight.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"\x1b[?1049h"); // enter the alternate screen
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    let highlighted_selection =
        get_selection(&mut server, client_id, pane_id).expect("highlighted");

    // The app scrolls on its own: cursor to the last row, a line feed there.
    server.handle_pty_output(pane_id, b"\x1b[999;1H\n");
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        Some(highlighted_selection),
        "the highlight is retained until input into the pane clears it"
    );
}

#[test]
fn a_screen_highlight_survives_the_app_moving_rows_around() {
    // The app deleting or inserting lines moves screen rows and can leave the
    // highlight over different text. The highlight stays where it was. The
    // next click or key into the pane clears it, and copy captures the text at
    // the drag's release.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    let highlighted_selection =
        get_selection(&mut server, client_id, pane_id).expect("highlighted");

    // DL with the cursor on row 3: rows below slide up, nothing is pushed.
    server.handle_pty_output(pane_id, b"\x1b[3;1H\x1b[M");
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        Some(highlighted_selection),
        "the highlight is retained after the app moved its rows"
    );
}

#[test]
fn a_history_highlight_survives_a_primary_row_shift() {
    // History rows do not move when screen rows do: their numbers still name
    // the same text. A highlight entirely in history stays.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    for line_number in 0..60 {
        server.handle_pty_output(pane_id, format!("line{line_number}\r\n").as_bytes());
    }

    // Scrolled up three lines, the top three view rows are history rows.
    server.scroll_up(client_id, pane_id, 3);
    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 1);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    let highlighted_selection =
        get_selection(&mut server, client_id, pane_id).expect("highlighted");

    server.handle_pty_output(pane_id, b"\x1b[10;1H\x1b[M");
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        Some(highlighted_selection),
        "a highlight entirely in history is untouched by screen row moves"
    );
}

#[test]
fn a_highlight_on_the_alternate_screen_survives_a_redraw_in_place() {
    // Rewriting cells without moving rows (how a full-screen app updates a
    // status line) leaves the highlight standing, the same as an in-place
    // redraw on the primary screen.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"\x1b[?1049h");
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    let highlighted_selection =
        get_selection(&mut server, client_id, pane_id).expect("highlighted");

    server.handle_pty_output(pane_id, b"\x1b[2;1Hredrawn text");
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        Some(highlighted_selection),
        "no rows moved, and the highlight is retained"
    );
}

#[test]
fn a_plain_click_copies_nothing() {
    // A click's press highlights nothing, and its release copies nothing: no
    // clipboard write, and the clipboard the user already had is untouched.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    assert_eq!(server.take_host_writes(client_id), None);
}

#[test]
fn releasing_the_gesture_is_the_copy() {
    // No copy key exists: releasing the selection is the copy. The highlighted
    // text goes to the client's outer terminal as OSC 52, which sets the OS
    // clipboard, and the highlight stays standing.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        server.take_host_writes(client_id),
        None,
        "nothing is copied while the drag is still moving"
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );

    // base64("hello") = aGVsbG8=
    assert_eq!(
        server
            .take_host_writes(client_id)
            .expect("queued clipboard write"),
        b"\x1b]52;c;aGVsbG8=\x07".to_vec()
    );
    let retained_selection = get_selection(&mut server, client_id, pane_id)
        .expect("the highlight stays; the exit rules end it as usual");
    assert_eq!(
        retained_selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        retained_selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );
}

#[test]
fn copy_release_obeys_disabled_trailing_whitespace_trimming() {
    let (mut server, viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut viewer = build_copying_viewer(
        &mut server,
        client_id,
        PartialCopyConfig {
            should_trim_trailing_whitespace: Some(false),
        },
    );
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"a");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_alt_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 2, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );

    assert_eq!(
        server
            .take_host_writes(client_id)
            .expect("queued clipboard write"),
        b"\x1b]52;c;YSAg\x07".to_vec()
    );
}

#[test]
fn ctrl_c_clears_the_highlight_like_any_key_reaching_the_pane() {
    // Ctrl+C binds nothing. It falls through to the shell (SIGINT) as input
    // reaching the pane's child, and the highlight clears.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    let highlighted_selection =
        get_selection(&mut server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted_selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted_selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );

    // `<C-c>` binds nothing. The viewer passes it through and the session
    // writes it to the pane.
    server.handle_key_input(
        client_id,
        &build_key_input_for_chord(KeyChord::from_parts(
            BindingModifierFlags::CTRL,
            Key::Char('c'),
        )),
    );
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        None,
        "Ctrl+C reached the shell and the highlight is gone"
    );
}

#[test]
fn typing_into_the_pane_clears_the_typists_highlight_there() {
    // Input reaching the pane's child ends the highlight. A key no binding
    // consumes is written to the child and clears the highlight.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    let highlighted_selection =
        get_selection(&mut server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted_selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted_selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );

    server.handle_key_input(
        client_id,
        &build_key_input_for_chord(KeyChord::from_parts(
            BindingModifierFlags::NONE,
            Key::Char('x'),
        )),
    );
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        None,
        "the key reached the child and the highlight is gone"
    );
}

#[test]
fn typing_during_a_drag_cancels_the_highlight_and_the_gesture() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    let highlighted_selection =
        get_selection(&mut server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted_selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted_selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );

    // The key reaches the pane's program. The viewer drops the gesture and the
    // session drops the highlight, in the order the binary's loop runs them.
    viewer.end_mouse_selection();
    server.handle_key_input(
        client_id,
        &build_key_input_for_chord(KeyChord::from_parts(
            BindingModifierFlags::NONE,
            Key::Char('x'),
        )),
    );
    assert_eq!(get_selection(&mut server, client_id, pane_id), None);

    let later_drag_point = get_pane_screen_cell(&server, client_id, pane_id, 6, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(later_drag_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        None,
        "the drag has no gesture behind it and highlights nothing"
    );
}

#[test]
fn typing_after_a_press_cancels_the_empty_gesture() {
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    assert_eq!(get_selection(&mut server, client_id, pane_id), None);

    viewer.end_mouse_selection();
    server.handle_key_input(
        client_id,
        &build_key_input_for_chord(KeyChord::from_parts(
            BindingModifierFlags::NONE,
            Key::Char('x'),
        )),
    );

    // The armed gesture is gone: a drag from here highlights nothing.
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    assert_eq!(get_selection(&mut server, client_id, pane_id), None);
}

#[test]
fn typing_leaves_another_panes_highlight_alone() {
    // Only the pane the key reaches loses its highlight. Another pane's
    // highlight stays.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let end_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    let highlighted_selection =
        get_selection(&mut server, client_id, pane_id).expect("highlighted");

    // The split focuses the new pane, and the key types into it.
    let other_pane_id = split_pane_rightward(&mut server, client_id);
    assert_ne!(other_pane_id, pane_id);
    server.handle_key_input(
        client_id,
        &build_key_input_for_chord(KeyChord::from_parts(
            BindingModifierFlags::NONE,
            Key::Char('x'),
        )),
    );
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        Some(highlighted_selection),
        "the highlight in the unfocused pane is retained"
    );
}

#[test]
fn a_click_forwarded_to_a_mouse_aware_program_clears_the_highlight() {
    // A click forwarded to a program that asked for mouse reports reaches the
    // child and clears the highlight.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    let highlighted_selection =
        get_selection(&mut server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted_selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted_selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );

    // The program turns mouse reporting on. The next press goes to the program.
    server.handle_pty_output(pane_id, b"\x1b[?1000h");
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        None,
        "the forwarded click reached the child and the highlight is gone"
    );
}

#[test]
fn a_press_on_a_scrolled_view_highlights_the_history_line_the_user_saw() {
    // The frame says which line each pane's top visible row is, and the viewer
    // anchors on that. A view scrolled ten lines back highlights the history
    // line under the pointer, not the live line that sits at the same screen
    // row at the bottom.
    //
    // Sixty `lineNN` writes on a 20-row pane push 41 lines into history: the
    // live view's top row is line 41, and a ten-line scroll back puts line 31
    // there. Output arriving between the paint and the press does not move it:
    // a scrolled view is held and the text it shows stays put.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    for line_number in 0..60 {
        server.handle_pty_output(pane_id, format!("line{line_number:02}\r\n").as_bytes());
    }
    server.scroll_up(client_id, pane_id, 10);

    // The frame the viewer is looking at, taken before the output arrives.
    let painted_mouse_frame =
        build_mouse_frame(server.build_snapshot(client_id).expect("snapshot"));
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);

    // Eight more lines land while the pointer is on its way down.
    for line_number in 0..8 {
        server.handle_pty_output(pane_id, format!("late{line_number}\r\n").as_bytes());
    }

    // The press and the drag are answered against the stale frame.
    for mouse_input in [
        build_mouse_press(screen_point),
        build_mouse_press(screen_point),
        build_mouse_press(screen_point),
        build_mouse_release(screen_point),
    ] {
        let mouse_actions = viewer.handle_mouse(
            mouse_input,
            &painted_mouse_frame,
            clock.advance_double_click_interval(),
        );
        apply_mouse_actions(
            &mut server,
            &mut viewer,
            &painted_mouse_frame,
            mouse_actions,
        );
    }

    let copied_bytes = server
        .take_host_writes(client_id)
        .expect("the release copied");
    assert_eq!(
        copied_bytes,
        b"\x1b]52;c;bGluZTMx\x07".to_vec(),
        "the highlight is on `line31`, the line the frame's top row showed, and \
         not on the live line that draws there when the view is at the bottom"
    );
}

/// Wraps `command` in an envelope from `client_id`'s mouse under `command_id`,
/// dispatches it to `server`, and returns the command's result.
fn dispatch_mouse_command(
    server: &mut Server,
    client_id: ClientId,
    command_id: CommandId,
    command: Command,
) -> CommandResult {
    server.dispatch(CommandEnvelope::from_parts(
        command_id,
        CommandSource::from_mouse(client_id),
        command,
    ))
}

#[test]
fn copying_a_highlight_reaching_past_the_last_line_reads_the_lines_that_are_there() {
    // A highlight's row numbers arrive on the command wire unbounded. A command
    // can name a row far past anything the pane has ever held. The copy reads
    // only the rows the pane has and answers at once.
    let (mut server, viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    server.handle_pty_output(pane_id, b"hello world");

    let set_command_id = CommandId::new();
    let set_command_result = dispatch_mouse_command(
        &mut server,
        client_id,
        set_command_id,
        Command::Visual(VisualCommand::SetSelection(SetSelectionArgs {
            pane_id,
            selection: Selection {
                selection_kind: SelectionKind::Character,
                anchor: GridPosition {
                    row_index: 0,
                    column_index: 0,
                },
                cursor: GridPosition {
                    row_index: u64::MAX,
                    column_index: u16::MAX,
                },
            },
        })),
    );
    let stored_selection =
        get_selection(&mut server, client_id, pane_id).expect("the highlight is stored");
    assert_eq!(
        set_command_result,
        CommandResult::Ok {
            command_id: set_command_id,
            emitted_events: vec![Event::SelectionChanged(SelectionChanged {
                client_id,
                pane_id,
                selection: Some(stored_selection),
            })],
        }
    );

    let copy_command_id = CommandId::new();
    assert_eq!(
        dispatch_mouse_command(
            &mut server,
            client_id,
            copy_command_id,
            Command::Visual(VisualCommand::Copy(CopyArgs {
                pane_id,
                should_trim_trailing_whitespace: true,
            })),
        ),
        CommandResult::Ok {
            command_id: copy_command_id,
            emitted_events: Vec::new(),
        },
        "a copy emits no event of its own"
    );

    // The pane holds one written row and blank rows under it. Every row after
    // the first ends hard and contributes a newline and no text.
    let content_row_count = find_pane_content_rect(&server, client_id, pane_id)
        .size
        .row_count;
    let blank_row_count = usize::from(content_row_count) - 1;
    let expected_clipboard_text = format!("hello world{}", "\n".repeat(blank_row_count));
    assert_eq!(
        server.take_host_writes(client_id),
        Some(crate::runtime::clipboard::encode_osc52_copy(
            &expected_clipboard_text
        ))
    );
}

#[test]
fn a_copy_writes_the_highlighted_text_to_the_outer_terminal_as_osc52() {
    let (mut server, viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    server.handle_pty_output(pane_id, b"hello");

    let highlight_selection = Selection {
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
    let set_command_id = CommandId::new();
    assert_eq!(
        dispatch_mouse_command(
            &mut server,
            client_id,
            set_command_id,
            Command::Visual(VisualCommand::SetSelection(SetSelectionArgs {
                pane_id,
                selection: highlight_selection,
            })),
        ),
        CommandResult::Ok {
            command_id: set_command_id,
            emitted_events: vec![Event::SelectionChanged(SelectionChanged {
                client_id,
                pane_id,
                selection: Some(highlight_selection),
            })],
        }
    );

    let copy_command_id = CommandId::new();
    assert_eq!(
        dispatch_mouse_command(
            &mut server,
            client_id,
            copy_command_id,
            Command::Visual(VisualCommand::Copy(CopyArgs {
                pane_id,
                should_trim_trailing_whitespace: true,
            })),
        ),
        CommandResult::Ok {
            command_id: copy_command_id,
            emitted_events: Vec::new(),
        }
    );
    // base64("hello") = aGVsbG8=
    assert_eq!(
        server.take_host_writes(client_id),
        Some(b"\x1b]52;c;aGVsbG8=\x07".to_vec()),
        "the same highlight copied to OSC 52 reaches the outer terminal"
    );
}

#[test]
fn a_drag_ends_when_its_pane_swaps_to_the_alternate_screen() {
    // The drag's anchor names a line of the primary screen's text. The gesture
    // ends when the pane swaps to the alternate screen, and the next motion
    // asks for nothing.
    let (mut server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    server.handle_pty_output(pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&server, client_id, pane_id, 0, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&server, client_id, pane_id, 4, 0);
    dispatch_mouse_input_at_time(
        &mut server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    let highlighted_selection =
        get_selection(&mut server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted_selection.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted_selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );

    // The program enters the alternate screen mid-drag (what `vim` does on
    // start). The session drops the highlight.
    server.handle_pty_output(pane_id, b"\x1b[?1049h");

    let drag_point_after_swap = get_pane_screen_cell(&server, client_id, pane_id, 8, 0);
    let mouse_frame = build_mouse_frame(server.build_snapshot(client_id).expect("snapshot"));
    let mouse_actions = viewer.handle_mouse(
        build_mouse_drag(drag_point_after_swap),
        &mouse_frame,
        clock.advance_one_second(),
    );

    assert_eq!(
        mouse_actions,
        Vec::new(),
        "no drag is under way and the motion asks for no highlight"
    );
    apply_mouse_actions(&mut server, &mut viewer, &mouse_frame, mouse_actions);
    assert_eq!(
        get_selection(&mut server, client_id, pane_id),
        None,
        "and nothing put a highlight back on the alternate screen"
    );
}
