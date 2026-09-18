//! Tests for selecting text with the mouse, driven through both halves: the
//! viewer resolves each gesture against the frame it painted and the session
//! stores, snaps, and copies what came back.

use super::*;

use std::collections::VecDeque;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};

use koshi_client::mouse::MouseAction;
use koshi_client::Client as ViewerClient;
use koshi_config::layer::{PartialCopyConfig, PartialKoshiConfig};
use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, CopyArgs, CopyTarget, GridPosition,
    NewPaneArgs, NewTabArgs, Selection, SelectionKind, SetSelectionArgs, VisualCommand,
};
use koshi_core::event::{Event, SelectionChanged};
use koshi_core::geometry::{Direction, Point, Size};
use koshi_core::ids::{CommandId, SessionId};
use koshi_core::key::{Key, KeyChord, ModFlags};
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::snapshot::{MouseFrame, ViewerChrome};
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::fixtures::build_key_input_for_chord;

use crate::runtime::bus::EventFilter;

/// A runtime with one bootstrapped 80x24 client, its viewer half, and its
/// single pane.
fn build_selection_runtime() -> (Server, ViewerClient, PaneId) {
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (event_sender, event_receiver) = mpsc::channel();
    let mut runtime_server =
        Server::from_runtime_parts(fake_pty_backend, event_receiver, event_sender);
    let client_id = runtime_server
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let pane_id = *runtime_server
        .pty_handle_by_pane_id
        .keys()
        .next()
        .expect("one pane");
    let viewer = build_viewer_for_client(&mut runtime_server, client_id);
    (runtime_server, viewer, pane_id)
}

/// The viewer half for `client`, on the stock settings.
fn build_viewer_for_client(runtime_server: &mut Server, client_id: ClientId) -> ViewerClient {
    ViewerClient::from_client_id_and_viewport(
        client_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
        runtime_server.subscribe(client_id, EventFilter::All),
        TerminalCleanupGuard::new(),
    )
}

/// The same viewer, with its `copy_config` settings overridden.
fn build_copying_viewer(
    runtime_server: &mut Server,
    client_id: ClientId,
    copy_config: PartialCopyConfig,
) -> ViewerClient {
    let mut viewer = build_viewer_for_client(runtime_server, client_id);
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

/// One mouse event, the way the running binary delivers it: the viewer decides
/// what it means against the frame it is looking at, and only what it decided
/// reaches the session.
fn process_mouse_input(
    runtime: &mut Server,
    viewer: &mut ViewerClient,
    mouse_input: MouseInput,
    now: Instant,
) {
    viewer.apply_events();
    let mouse_frame = MouseFrame::from(
        runtime
            .build_snapshot(viewer.get_client_id())
            .expect("snapshot"),
    );
    let mouse_actions = viewer.handle_mouse(mouse_input, &mouse_frame, now);
    apply_mouse_actions(runtime, viewer, &mouse_frame, mouse_actions);
}

/// Fire the selection drag's scroll timer, as the binary's loop does.
fn expire_mouse_scroll(runtime_server: &mut Server, viewer: &mut ViewerClient, now: Instant) {
    let mouse_frame = MouseFrame::from(
        runtime_server
            .build_snapshot(viewer.get_client_id())
            .expect("snapshot"),
    );
    let mouse_actions = viewer.expire_mouse_scroll(now, &mouse_frame);
    apply_mouse_actions(runtime_server, viewer, &mouse_frame, mouse_actions);
}

/// Run everything the viewer decided, the way the binary's loop does.
fn apply_mouse_actions(
    runtime_server: &mut Server,
    viewer: &mut ViewerClient,
    mouse_frame: &MouseFrame,
    mouse_actions: Vec<MouseAction>,
) {
    let client_id = viewer.get_client_id();
    let mut mouse_action_queue: VecDeque<MouseAction> = mouse_actions.into();
    while let Some(mouse_action) = mouse_action_queue.pop_front() {
        match mouse_action {
            MouseAction::Scroll {
                pane_id,
                is_scrolling_up,
                scroll_line_count,
            } => {
                let top_row_index = runtime_server.scroll_pane_view(
                    client_id,
                    pane_id,
                    is_scrolling_up,
                    scroll_line_count,
                );
                mouse_action_queue.extend(viewer.note_scroll_applied(
                    pane_id,
                    top_row_index,
                    mouse_frame,
                ));
            }
            MouseAction::Forward {
                pane_id,
                mouse_input,
            } => {
                let is_written =
                    runtime_server.forward_mouse_to_pane(client_id, pane_id, mouse_input);
                if let (true, MouseKind::Press(mouse_button)) = (is_written, mouse_input.mouse_kind)
                {
                    viewer.note_press_forwarded(pane_id, mouse_button);
                }
            }
            MouseAction::AltScrollArrows {
                pane_id,
                is_scrolling_up,
                arrow_count,
            } => {
                runtime_server.write_alt_scroll_arrows(pane_id, is_scrolling_up, arrow_count);
            }
            MouseAction::Resize {
                pane_id,
                border_side,
                resize_step,
                requested_cell_count,
            } => {
                let applied_cell_count = runtime_server.drag_resize(
                    client_id,
                    pane_id,
                    border_side,
                    resize_step,
                    requested_cell_count,
                );
                viewer.note_resize_applied(pane_id, border_side, resize_step, applied_cell_count);
            }
            MouseAction::Command(command) => {
                let envelope = CommandEnvelope::from_parts(
                    CommandId::new(),
                    CommandSource::from_mouse(client_id),
                    SystemTime::now(),
                    command,
                );
                let _ = runtime_server.submit_command(envelope);
            }
        }
    }
}

/// Feed `output_bytes` into `pane`'s terminal, so its screen has text to select.
fn feed_terminal_output(runtime_server: &mut Server, pane_id: PaneId, output_bytes: &[u8]) {
    runtime_server.handle_pty_output(pane_id, output_bytes);
}

/// The screen origin of `pane`'s content area: the cell its row 0, column 0
/// draws at.
fn get_pane_content_origin(runtime_server: &Server, client_id: ClientId, pane_id: PaneId) -> Point {
    let snapshot = runtime_server.build_snapshot(client_id).expect("snapshot");
    koshi_renderer::pane_content_rect(
        snapshot.build_frame_layout(ViewerChrome::default()),
        pane_id,
    )
    .expect("content rect")
    .origin
}

/// The screen cell for `pane`'s content row `row_index`, column `column_index`.
fn get_pane_screen_cell(
    runtime: &Server,
    client_id: ClientId,
    pane_id: PaneId,
    column_index: u16,
    row_index: u16,
) -> Point {
    let origin = get_pane_content_origin(runtime, client_id, pane_id);
    Point {
        column: origin.column + column_index,
        row: origin.row + row_index,
    }
}

/// `pane`'s last content column. Derived, not assumed: the pane's border ring
/// eats into the client's viewport, so a pane in an 80-column terminal is
/// narrower than 80.
fn get_last_pane_content_column_index(
    runtime_server: &Server,
    client_id: ClientId,
    pane_id: PaneId,
) -> u16 {
    let snapshot = runtime_server.build_snapshot(client_id).expect("snapshot");
    koshi_renderer::pane_content_rect(
        snapshot.build_frame_layout(ViewerChrome::default()),
        pane_id,
    )
    .expect("content rect")
    .cell_size
    .column_count
        - 1
}

fn build_mouse_press(position: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Press(MouseButton::Left),
        position,
        modifier_flags: ModFlags::NONE,
    }
}

fn build_alt_mouse_press(position: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Press(MouseButton::Left),
        position,
        modifier_flags: ModFlags::ALT,
    }
}

fn build_mouse_drag(position: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Drag(MouseButton::Left),
        position,
        modifier_flags: ModFlags::NONE,
    }
}

fn build_mouse_release(position: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Release(MouseButton::Left),
        position,
        modifier_flags: ModFlags::NONE,
    }
}

/// Turn on this client's mouse-select mode the way its keybinding does, so a
/// drag grabs the mouse for a koshi selection even over a program that asked
/// for the mouse. The viewer picks the change up from its subscription on the
/// next event.
fn enable_mouse_selection(runtime_server: &mut Server, client_id: ClientId) {
    let _ = runtime_server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        SystemTime::now(),
        Command::ToggleMouseSelect,
    ));
}

/// A clock whose every reading is a second after the last, so no two presses
/// fall inside the click threshold unless a test asks them to.
struct Clock(Instant);

impl Clock {
    fn new() -> Self {
        Clock(Instant::now())
    }

    /// A second on from the last reading: two presses apart.
    fn advance_one_second(&mut self) -> Instant {
        self.0 += Duration::from_secs(1);
        self.0
    }

    /// A tenth of a second on: inside the 400ms threshold, so a second press
    /// here is a double click.
    fn advance_double_click_interval(&mut self) -> Instant {
        self.0 += Duration::from_millis(100);
        self.0
    }

    /// The last reading, without moving the clock on.
    fn get_current_time(&self) -> Instant {
        self.0
    }
}

/// This client's highlight in `pane`.
fn get_selection(
    runtime_server: &mut Server,
    client_id: ClientId,
    pane_id: PaneId,
) -> Option<Selection> {
    runtime_server
        .get_client_mut(client_id)
        .expect("client")
        .get_selection(pane_id)
}

/// Split the focused pane rightward and return the new pane's id.
fn split_pane_rightward(runtime_server: &mut Server, client_id: ClientId) -> PaneId {
    let existing_pane_ids: Vec<PaneId> = runtime_server
        .pty_handle_by_pane_id
        .keys()
        .copied()
        .collect();
    let envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        SystemTime::now(),
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
    let _ = runtime_server.dispatch(envelope);
    *runtime_server
        .pty_handle_by_pane_id
        .keys()
        .find(|pane_id| !existing_pane_ids.contains(pane_id))
        .expect("a new pane")
}

#[test]
fn a_drag_highlights_from_the_press_to_the_pointer() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 6, 0); // the `w`
    let end_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 10, 0); // the `d`
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );

    let selection = get_selection(&mut runtime_server, client_id, pane_id).expect("a highlight");
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
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    // A plain click: press and release, the pointer never moving. Nothing is
    // highlighted, and in particular no empty highlight is left to hold the view.
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 3, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );

    assert_eq!(get_selection(&mut runtime_server, client_id, pane_id), None);
    assert!(
        !runtime_server
            .get_client_mut(client_id)
            .expect("client")
            .is_view_held(pane_id),
        "a click leaves the view following live output"
    );
}

#[test]
fn a_press_drops_the_highlight_that_was_up() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    let highlighted = get_selection(&mut runtime_server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );

    // Pressing again clears it.
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    assert_eq!(get_selection(&mut runtime_server, client_id, pane_id), None);
}

#[test]
fn a_drag_in_one_pane_leaves_the_other_panes_highlight_alone() {
    let (mut runtime_server, mut viewer, initial_pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    let split_pane_id = split_pane_rightward(&mut runtime_server, client_id);
    feed_terminal_output(&mut runtime_server, initial_pane_id, b"first pane");
    feed_terminal_output(&mut runtime_server, split_pane_id, b"second pane");

    // Highlight in the second pane (the split focused it).
    let start_point = get_pane_screen_cell(&runtime_server, client_id, split_pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&runtime_server, client_id, split_pane_id, 5, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    let split_pane_selection = get_selection(&mut runtime_server, client_id, split_pane_id)
        .expect("second pane highlighted");

    // Now focus the first pane and highlight there. A click on an unfocused pane
    // only focuses, so it takes a second press to start the drag.
    let initial_pane_from = get_pane_screen_cell(&runtime_server, client_id, initial_pane_id, 0, 0);
    let initial_pane_to = get_pane_screen_cell(&runtime_server, client_id, initial_pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(initial_pane_from),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(initial_pane_from),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(initial_pane_to),
        clock.advance_one_second(),
    );

    let initial_pane_selection = get_selection(&mut runtime_server, client_id, initial_pane_id)
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
        get_selection(&mut runtime_server, client_id, split_pane_id),
        Some(split_pane_selection),
        "the second pane's highlight is exactly as it was"
    );
}

#[test]
fn a_focus_click_on_another_pane_clears_no_highlight() {
    let (mut runtime_server, mut viewer, initial_pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    let split_pane_id = split_pane_rightward(&mut runtime_server, client_id);
    feed_terminal_output(&mut runtime_server, initial_pane_id, b"first pane");
    feed_terminal_output(&mut runtime_server, split_pane_id, b"second pane");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, split_pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&runtime_server, client_id, split_pane_id, 5, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    let split_pane_selection = get_selection(&mut runtime_server, client_id, split_pane_id)
        .expect("second pane highlighted");

    // One click on the other pane: a koshi focus trigger, which never reaches
    // the highlighted pane's program, so it clears nothing.
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, initial_pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );

    assert_eq!(
        get_selection(&mut runtime_server, client_id, split_pane_id),
        Some(split_pane_selection),
        "focusing away leaves the highlight up"
    );
}

#[test]
fn mouse_select_mode_selects_over_a_mouse_aware_program() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");
    // The program turns on mouse tracking: bare gestures are now its own.
    feed_terminal_output(&mut runtime_server, pane_id, b"\x1b[?1000h\x1b[?1006h");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);

    // With mouse-select off, the drag is forwarded to the program: no highlight.
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(end_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        None,
        "with mouse-select off, a drag over a mouse-aware program does not select"
    );

    // Turn mouse-select on: the same gesture now highlights in koshi.
    enable_mouse_selection(&mut runtime_server, client_id);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(end_point),
        clock.advance_one_second(),
    );
    let selection = get_selection(&mut runtime_server, client_id, pane_id)
        .expect("mouse-select drag highlights");
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
    let (mut runtime_server, mut viewer, initial_pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    let split_pane_id = split_pane_rightward(&mut runtime_server, client_id); // the split focuses `split_pane_id`
    feed_terminal_output(&mut runtime_server, initial_pane_id, b"first pane");
    feed_terminal_output(&mut runtime_server, split_pane_id, b"second pane");
    enable_mouse_selection(&mut runtime_server, client_id);

    let start_point = get_pane_screen_cell(&runtime_server, client_id, initial_pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&runtime_server, client_id, initial_pane_id, 4, 0);

    // The focus-first rule runs before mouse-select is consulted: the first
    // press on the unfocused pane only focuses it.
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        get_selection(&mut runtime_server, client_id, initial_pane_id),
        None,
        "the focusing press does not also select"
    );

    // Now focused, a second press then drag highlights.
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    let highlighted = get_selection(&mut runtime_server, client_id, initial_pane_id)
        .expect("the second drag selects in the now-focused pane");
    assert_eq!(
        highlighted.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );
}

#[test]
fn a_double_click_drag_snaps_to_whole_words() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    // Two presses inside the threshold, then a drag: word selection.
    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 2, 0); // inside `hello`
    let end_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 8, 0); // inside `world`
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_double_click_interval(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_double_click_interval(),
    );

    let selection = get_selection(&mut runtime_server, client_id, pane_id).expect("a highlight");
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
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_double_click_interval(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_double_click_interval(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_double_click_interval(),
    );

    let last_column_index = get_last_pane_content_column_index(&runtime_server, client_id, pane_id);
    let selection = get_selection(&mut runtime_server, client_id, pane_id).expect("a highlight");
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
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    for _ in 0..4 {
        process_mouse_input(
            &mut runtime_server,
            &mut viewer,
            build_mouse_press(screen_point),
            clock.advance_double_click_interval(),
        );
    }
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 6, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_double_click_interval(),
    );

    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id)
            .expect("a highlight")
            .selection_kind,
        SelectionKind::Character,
        "the run wraps back to a single click after three"
    );
}

#[test]
fn two_slow_clicks_are_two_single_clicks() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    // A second apart: well past the 400ms threshold, so this is not a double
    // click and the drag selects characters, not the whole word.
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 2, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 3, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );

    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id)
            .expect("a highlight")
            .selection_kind,
        SelectionKind::Character
    );
}

#[test]
fn alt_held_at_the_press_makes_a_block() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"one\r\ntwo\r\nthree");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 1, 0);
    let end_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 2, 2);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_alt_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );

    let selection = get_selection(&mut runtime_server, client_id, pane_id).expect("a highlight");
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
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    // A rectangle is a different shape, not a different amount of text, so it
    // does not compete with the run of clicks.
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 2, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_alt_mouse_press(screen_point),
        clock.advance_double_click_interval(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_double_click_interval(),
    );

    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id)
            .expect("a highlight")
            .selection_kind,
        SelectionKind::Block
    );
}

#[test]
fn a_release_ends_the_drag_but_leaves_the_highlight() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(end_point),
        clock.advance_one_second(),
    );

    let after_release = get_selection(&mut runtime_server, client_id, pane_id)
        .expect("the highlight retained_selection");

    // A later drag with no press behind it extends nothing.
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 8, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        Some(after_release),
        "a drag with no press behind it changes nothing"
    );
}

#[test]
fn a_highlight_holds_the_view_at_the_live_bottom() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );

    assert!(
        runtime_server
            .get_client_mut(client_id)
            .expect("client")
            .is_view_held(pane_id),
        "a highlight holds the view even at offset 0, which an offset alone \
         cannot express"
    );
}

#[test]
fn a_drag_past_the_bottom_edge_scrolls_and_keeps_extending() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    // Fill well past the 24-row screen so there is history to scroll into.
    for line_number in 0..60 {
        feed_terminal_output(
            &mut runtime_server,
            pane_id,
            format!("line{line_number}\r\n").as_bytes(),
        );
    }

    // Scroll up so there is somewhere to scroll back down to, then drag past the
    // bottom edge.
    runtime_server.scroll_up(client_id, pane_id, 10);
    let scroll_offset_before_timer = runtime_server
        .get_client_mut(client_id)
        .expect("client")
        .get_scroll_offset(pane_id);
    assert_eq!(scroll_offset_before_timer, 10);

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let below = Point {
        column: start_point.column,
        row: get_pane_content_origin(&runtime_server, client_id, pane_id).row + 100, // far below the pane_id
    };
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(below),
        clock.advance_one_second(),
    );
    let during_drag =
        get_selection(&mut runtime_server, client_id, pane_id).expect("the drag highlights");

    // The drag armed the scroll rather than scrolling on the event itself.
    assert_eq!(
        viewer.next_mouse_wakeup(clock.get_current_time()),
        Some(Duration::from_millis(15)),
        "a pointer past the bottom last column arms the scroll timer"
    );

    // Firing the timer scrolls one line toward live output and keeps the
    // highlight extending, without any further mouse event.
    expire_mouse_scroll(&mut runtime_server, &mut viewer, clock.advance_one_second());
    assert_eq!(
        runtime_server
            .get_client_mut(client_id)
            .expect("client")
            .get_scroll_offset(pane_id),
        scroll_offset_before_timer - 1,
        "one line per firing, toward the pointer"
    );
    let selection_after_timer =
        get_selection(&mut runtime_server, client_id, pane_id).expect("and the highlight keeps up");
    assert_eq!(
        selection_after_timer.anchor, during_drag.anchor,
        "the anchor stays on the cell the press named"
    );
    assert_eq!(
        selection_after_timer.cursor.row_index,
        during_drag.cursor.row_index + 1,
        "and the moving end followed the view one line down"
    );
}

#[test]
fn a_pointer_back_inside_the_pane_stops_the_scrolling() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    for line_number in 0..60 {
        feed_terminal_output(
            &mut runtime_server,
            pane_id,
            format!("line{line_number}\r\n").as_bytes(),
        );
    }
    runtime_server.scroll_up(client_id, pane_id, 10);

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let below = Point {
        column: start_point.column,
        row: get_pane_content_origin(&runtime_server, client_id, pane_id).row + 100,
    };
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(below),
        clock.advance_one_second(),
    );
    assert_eq!(
        viewer.next_mouse_wakeup(clock.get_current_time()),
        Some(Duration::from_millis(15)),
        "the overshoot arms the scroll"
    );

    // Back inside: the scroll disarms and the view stops moving on its own.
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 2);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        viewer.next_mouse_wakeup(clock.get_current_time()),
        None,
        "a pointer inside the pane does not scroll"
    );

    let held_selection = runtime_server
        .get_client_mut(client_id)
        .expect("client")
        .get_scroll_offset(pane_id);
    expire_mouse_scroll(&mut runtime_server, &mut viewer, clock.advance_one_second());
    assert_eq!(
        runtime_server
            .get_client_mut(client_id)
            .expect("client")
            .get_scroll_offset(pane_id),
        held_selection,
        "a disarmed drag scrolls nothing when the timer runs"
    );
}

#[test]
fn a_wakeup_is_asked_for_only_while_a_drag_is_held_past_an_edge() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");
    let now = clock.advance_one_second();

    assert_eq!(
        viewer.next_mouse_wakeup(now),
        None,
        "an idle client asks for no wakeup"
    );

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        viewer.next_mouse_wakeup(clock.advance_one_second()),
        None,
        "a drag inside the pane asks for no wakeup"
    );

    let below = Point {
        column: start_point.column,
        row: get_pane_content_origin(&runtime_server, client_id, pane_id).row + 100,
    };
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(below),
        clock.advance_one_second(),
    );
    assert_eq!(
        viewer.next_mouse_wakeup(clock.advance_one_second()),
        Some(Duration::ZERO),
        "a drag past the last column asks the loop to wake, and a second on is overdue"
    );
}

#[test]
fn switching_to_the_alternate_screen_drops_the_highlight() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    let highlighted = get_selection(&mut runtime_server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );

    // The program enters the alternate screen (what `vim` does on start). A row
    // number counts lines pushed into scrollback, which the alternate screen has
    // none of, so the highlight names nothing there.
    feed_terminal_output(&mut runtime_server, pane_id, b"\x1b[?1049h");

    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        None,
        "the highlight went with the screen"
    );
    assert!(
        !runtime_server
            .get_client_mut(client_id)
            .expect("client")
            .is_view_held(pane_id),
        "and the view is no longer held_selection by it"
    );
}

#[test]
fn a_drag_beyond_the_last_column_clamps_to_the_edge() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    // Far to the right of the pane: there is no more text sideways, so the
    // highlight stops at the last column rather than running on.
    let right_of = Point {
        column: get_pane_content_origin(&runtime_server, client_id, pane_id).column + 200,
        row: start_point.row,
    };
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(right_of),
        clock.advance_one_second(),
    );

    let last_column_index = get_last_pane_content_column_index(&runtime_server, client_id, pane_id);
    let selection = get_selection(&mut runtime_server, client_id, pane_id).expect("a highlight");
    assert_eq!(
        selection.cursor,
        GridPosition {
            row_index: 0,
            column_index: last_column_index
        }
    );
    assert_eq!(
        viewer.next_mouse_wakeup(clock.get_current_time()),
        None,
        "a sideways overshoot does not scroll"
    );
}

#[test]
fn a_drag_up_leaves_the_anchor_after_the_cursor() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"one\r\ntwo\r\nthree");

    // Press on the third row and drag up to the first: the anchor stays where
    // the press was, so it is the later end.
    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 2, 2);
    let end_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 1, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );

    let selection = get_selection(&mut runtime_server, client_id, pane_id).expect("a highlight");
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
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    let held_selection =
        get_selection(&mut runtime_server, client_id, pane_id).expect("highlighted");

    // A new tab takes the client's view with it, so the drag's pane is no
    // longer on the frame.
    let envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        SystemTime::now(),
        Command::NewTab(NewTabArgs::default()),
    );
    let _ = runtime_server.dispatch(envelope);

    // A further drag extends nothing: the gesture went with the tab.
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        Some(held_selection),
        "the highlight belongs to its pane and is found again on switching back"
    );
}

#[test]
fn output_under_a_highlight_leaves_it_on_the_same_text_and_the_same_screen_row() {
    // The point of numbering rows absolutely: the highlight is stored once and
    // never re-anchored, yet output arriving underneath moves neither the text
    // it names nor where it draws.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"target line\r\n");

    // Highlight `target` on the first row.
    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 5, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    let selection_before_output =
        get_selection(&mut runtime_server, client_id, pane_id).expect("a highlight");
    let drawn_before = get_drawn_row_spans(&runtime_server, client_id);

    // Enough output to push the highlighted line off the top of the screen.
    for line_number in 0..30 {
        feed_terminal_output(
            &mut runtime_server,
            pane_id,
            format!("noise{line_number}\r\n").as_bytes(),
        );
    }

    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        Some(selection_before_output),
        "the stored highlight was never touched"
    );
    assert_eq!(
        get_drawn_row_spans(&runtime_server, client_id),
        drawn_before,
        "and it still draws on the same screen row: the view was held_selection, so the \
         text under it did not move"
    );
}

/// The highlight rows the client's first pane draws this frame.
fn get_drawn_row_spans(
    runtime_server: &Server,
    client_id: ClientId,
) -> Option<Vec<(u16, u16, u16)>> {
    let snap = runtime_server.build_snapshot(client_id).expect("snapshot");
    snap.pane_snapshots[0]
        .selection_spans
        .as_ref()
        .map(|selection_spans| selection_spans.row_spans.clone())
}

#[test]
fn a_word_on_the_alternate_screen_never_reaches_into_the_primarys_history() {
    // The alternate screen keeps no scrollback of its own. Growing a word from
    // its top row must stop there, not walk up into the lines the PRIMARY
    // pushed into history — those are a different screen's text entirely.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();

    // A single very long line on the primary, wrapping many times, so the rows
    // it pushes into history all end SOFT — the text "continues" onto the row
    // below, which is what makes the walk cross the boundary.
    feed_terminal_output(&mut runtime_server, pane_id, "x".repeat(78 * 40).as_bytes());
    // Enter the alternate screen (which does NOT clear the primary's history)
    // and write a word on it. `ab ` puts a separator before `foo`, so the word
    // genuinely starts at column 3 — anything reaching further left has crossed
    // into text that is not on this screen.
    feed_terminal_output(&mut runtime_server, pane_id, b"\x1b[?47h");
    feed_terminal_output(&mut runtime_server, pane_id, b"ab foo bar");

    let primary_history_line_count = runtime_server
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
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_double_click_interval(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 5, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_double_click_interval(),
    );

    let selection = get_selection(&mut runtime_server, client_id, pane_id).expect("a highlight");
    assert!(
        selection.anchor.row_index >= primary_history_line_count,
        "the word anchored at row {} — below the alternate screen's first row \
         ({primary_history_line_count}), i.e. inside the primary's scrollback",
        selection.anchor.row_index
    );
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
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 8, 0); // inside `world`
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_double_click_interval(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_double_click_interval(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_double_click_interval(),
    );

    let selection =
        get_selection(&mut runtime_server, client_id, pane_id).expect("`world` is highlighted");
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
    // Same gesture family as the double click: complete without a drag.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    for _ in 0..3 {
        process_mouse_input(
            &mut runtime_server,
            &mut viewer,
            build_mouse_press(screen_point),
            clock.advance_double_click_interval(),
        );
        process_mouse_input(
            &mut runtime_server,
            &mut viewer,
            build_mouse_release(screen_point),
            clock.advance_double_click_interval(),
        );
    }

    let last_column_index = get_last_pane_content_column_index(&runtime_server, client_id, pane_id);
    let selection =
        get_selection(&mut runtime_server, client_id, pane_id).expect("the line is highlighted");
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
    // The press highlights the word; dragging on keeps extending by whole words
    // from the same anchor, rather than restarting.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 2, 0); // inside `hello`
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_double_click_interval(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_double_click_interval(),
    );
    let selection_after_press =
        get_selection(&mut runtime_server, client_id, pane_id).expect("`hello` is highlighted");
    assert_eq!(
        selection_after_press.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        },
        "just `hello`"
    );

    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 8, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_double_click_interval(),
    );
    let selection_after_drag =
        get_selection(&mut runtime_server, client_id, pane_id).expect("still highlighted");
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
    // The trap the single-click rule exists to avoid, checked for the gesture
    // that now highlights at the press: a double click on blank cells must not
    // leave a highlight that holds the view with nothing to show.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hi");

    // Column 40 is blank space well past the text.
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 40, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_double_click_interval(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_double_click_interval(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_double_click_interval(),
    );

    // Blanks are separators, and a click on a separator selects the run of that
    // character: every blank from the end of `hi` to the row's edge — a real
    // highlight, not an empty one.
    let selection = get_selection(&mut runtime_server, client_id, pane_id)
        .expect("the blank run under the pointer");
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
            column_index: get_last_pane_content_column_index(&runtime_server, client_id, pane_id),
        },
        "and reaches the row's last column"
    );
    assert!(
        runtime_server
            .get_client_mut(client_id)
            .expect("client")
            .is_view_held(pane_id),
        "a real highlight holds the view, and a click clears it again"
    );
}

#[test]
fn a_drag_past_the_top_edge_scrolls_into_history_and_keeps_extending() {
    // The reachable half of edge scrolling: from the live bottom the only way
    // the view can move is UP into history, so this is the path a person
    // actually takes. (Dragging past the bottom does nothing at offset 0 —
    // there is nowhere further down to go.)
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    for line_number in 0..60 {
        feed_terminal_output(
            &mut runtime_server,
            pane_id,
            format!("line{line_number}\r\n").as_bytes(),
        );
    }

    assert_eq!(
        runtime_server
            .get_client_mut(client_id)
            .expect("client")
            .get_scroll_offset(pane_id),
        0,
        "starts at the live bottom, where a person starts"
    );

    // Press inside the pane, then drag above its top edge and hold there.
    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 5);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let initial_selection = get_selection(&mut runtime_server, client_id, pane_id);
    assert_eq!(
        initial_selection, None,
        "a single-click press highlights nothing yet"
    );

    let above = Point {
        column: start_point.column,
        row: get_pane_content_origin(&runtime_server, client_id, pane_id)
            .row
            .saturating_sub(3),
    };
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(above),
        clock.advance_one_second(),
    );

    let held_selection = get_selection(&mut runtime_server, client_id, pane_id)
        .expect("dragging out of the primary_history_line_count highlights");
    let anchor_row_index = held_selection.anchor.row_index;

    // Three timer firings scroll three lines into history, with no further
    // mouse event — the pointer is held still.
    for _ in 0..3 {
        expire_mouse_scroll(&mut runtime_server, &mut viewer, clock.advance_one_second());
    }

    assert_eq!(
        runtime_server
            .get_client_mut(client_id)
            .expect("client")
            .get_scroll_offset(pane_id),
        3,
        "the view walked three lines up into history, one per firing"
    );
    let selection_after_scroll =
        get_selection(&mut runtime_server, client_id, pane_id).expect("still highlighted");
    assert_eq!(
        selection_after_scroll.anchor.row_index, anchor_row_index,
        "the initial_selection names the same line it always did — absolute rows do not \
         move when the view does"
    );
    assert_eq!(
        selection_after_scroll.cursor.row_index,
        anchor_row_index - 8,
        "and the moving end reached three lines further back than the primary_history_line_count row \
         it started on (5 rows up end_point the primary_history_line_count, then 3 into history)"
    );
}

#[test]
fn two_clients_selecting_in_one_pane_never_see_each_others_highlight() {
    // The load-bearing per-client claim: highlights live on the Client, so two
    // terminals viewing the same pane select independently and neither sees the
    // other's. This is the axis that a per-pane-only model (zellij's) gets wrong.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let first_client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    // A second client attached to the same session, viewing the same tab.
    let session_id = runtime_server
        .get_session_for_client(first_client_id)
        .expect("first client's session")
        .session_id;
    let tab_id = runtime_server
        .get_client_mut(first_client_id)
        .expect("first client")
        .get_active_tab();
    let second_client_id = ClientId::new();
    let mut bob_client = koshi_session::client::Client::from_attachment(
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
    bob_client.update_focused_pane(tab_id, pane_id);
    runtime_server
        .session_by_id
        .get_mut(&session_id)
        .expect("session")
        .attach_client(bob_client);

    // Alice highlights.
    let start_point = get_pane_screen_cell(&runtime_server, first_client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, first_client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );

    let first_client_selection = get_selection(&mut runtime_server, first_client_id, pane_id)
        .expect("first client has a highlight");
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
        get_selection(&mut runtime_server, second_client_id, pane_id),
        None,
        "second client, on the same pane, has none"
    );
    // And it is invisible in bob's own frame, which is what he actually sees.
    assert_eq!(
        runtime_server
            .build_snapshot(second_client_id)
            .expect("second client's frame")
            .pane_snapshots[0]
            .selection_spans,
        None,
        "second client's frame draws no highlight"
    );
    assert_eq!(
        runtime_server
            .build_snapshot(first_client_id)
            .expect("first client's frame")
            .pane_snapshots[0]
            .selection_spans
            .as_ref()
            .expect("first client's frame draws first_client_selection")
            .row_spans,
        vec![(0, 0, 4)],
        "one row, columns 0 to 4 inclusive"
    );
    // Alice's highlight holds only alice's view.
    assert!(runtime_server
        .get_client_mut(first_client_id)
        .expect("first client")
        .is_view_held(pane_id));
    assert!(
        !runtime_server
            .get_client_mut(second_client_id)
            .expect("second client")
            .is_view_held(pane_id),
        "second client's view of the same pane still follows live output"
    );
}

#[test]
fn a_drag_past_a_corner_scrolls_and_clamps_the_column() {
    // Past the top edge AND left of it at once: the vertical part scrolls, the
    // horizontal part just clamps — there is no more text sideways.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    for line_number in 0..60 {
        feed_terminal_output(
            &mut runtime_server,
            pane_id,
            format!("line{line_number}\r\n").as_bytes(),
        );
    }

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 10, 5);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let corner = Point {
        column: get_pane_content_origin(&runtime_server, client_id, pane_id)
            .column
            .saturating_sub(5),
        row: get_pane_content_origin(&runtime_server, client_id, pane_id)
            .row
            .saturating_sub(5),
    };
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(corner),
        clock.advance_one_second(),
    );

    let selection = get_selection(&mut runtime_server, client_id, pane_id).expect("a highlight");
    assert_eq!(
        selection.cursor.column_index, 0,
        "clamped to the first column"
    );
    assert_eq!(
        viewer.next_mouse_wakeup(clock.get_current_time()),
        Some(Duration::from_millis(15)),
        "and the vertical overshoot still arms the scroll"
    );
}

#[test]
fn erasing_all_history_under_a_highlight_drops_it_and_frees_the_view() {
    // A highlight whose every line has been erased (`CSI 3 J`) can never draw
    // again, but it would still hold the view against live output with nothing
    // on screen to explain why. It must go.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 6, 0); // the `w`
    let end_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 10, 0); // the `d`
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(end_point),
        clock.advance_one_second(),
    );
    let highlighted = get_selection(&mut runtime_server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted.anchor,
        GridPosition {
            row_index: 0,
            column_index: 6
        }
    );
    assert_eq!(
        highlighted.cursor,
        GridPosition {
            row_index: 0,
            column_index: 10
        }
    );

    // Sixty lines of output push `hello world` into history; the held view's
    // offset rises with it.
    for line_number in 0..60 {
        feed_terminal_output(
            &mut runtime_server,
            pane_id,
            format!("line{line_number}\r\n").as_bytes(),
        );
    }
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        Some(highlighted),
        "output alone never clears a highlight"
    );

    // The child erases its scrollback. Every line under the highlight is gone.
    feed_terminal_output(&mut runtime_server, pane_id, b"\x1b[3J");
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        None,
        "a highlight with nothing left to name is dropped"
    );
    assert!(
        !runtime_server
            .get_client_mut(client_id)
            .expect("client")
            .is_view_held(pane_id),
        "and the view follows live output again"
    );
}

#[test]
fn a_highlight_still_partly_on_screen_survives_a_history_erase() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    for line_number in 0..60 {
        feed_terminal_output(
            &mut runtime_server,
            pane_id,
            format!("line{line_number}\r\n").as_bytes(),
        );
    }

    // Scrolled up three lines, the top three view rows are history rows; a drag
    // from the top row down onto the live screen spans the boundary.
    runtime_server.scroll_up(client_id, pane_id, 3);
    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    let end_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 10);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(end_point),
        clock.advance_one_second(),
    );
    let selection_before_history_erase =
        get_selection(&mut runtime_server, client_id, pane_id).expect("a highlight");

    feed_terminal_output(&mut runtime_server, pane_id, b"\x1b[3J");
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        Some(selection_before_history_erase),
        "a highlight with lines still on the live screen keeps them"
    );
}

#[test]
fn a_press_on_the_right_half_of_a_wide_glyph_names_the_glyph_itself() {
    // The pointer can land on the blank right half of a wide (CJK) glyph, a
    // width-0 cell the renderer never paints. The position must name the
    // glyph's own cell, or a highlight could cover only invisible cells.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, "世界x".as_bytes());

    // 世 covers columns 0-1, 界 columns 2-3, x column 4. Press on 世's blank
    // half, drag onto 界's blank half.
    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 1, 0);
    let end_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 3, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );

    let selection = get_selection(&mut runtime_server, client_id, pane_id).expect("a highlight");
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
    // At the live bottom a drag held below the pane has nothing to scroll
    // toward. The timer must stop rather than fire every 15ms doing nothing;
    // the next drag event re-arms it.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let below = Point {
        column: start_point.column,
        row: get_pane_content_origin(&runtime_server, client_id, pane_id).row + 40,
    };
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(below),
        clock.advance_one_second(),
    );
    let now = clock.advance_one_second();
    assert_eq!(
        viewer.next_mouse_wakeup(now),
        Some(Duration::ZERO),
        "the overshoot arms the scroll, and a second on it is overdue"
    );

    // The firing finds the view already at the live bottom and moves nothing.
    expire_mouse_scroll(
        &mut runtime_server,
        &mut viewer,
        now + Duration::from_millis(15),
    );
    assert_eq!(
        viewer.next_mouse_wakeup(now + Duration::from_millis(15)),
        None,
        "a firing that moved nothing disarms the timer"
    );

    // The pointer moving again — still below the pane — re-arms it.
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(below),
        clock.advance_one_second(),
    );
    assert_eq!(
        viewer.next_mouse_wakeup(clock.advance_one_second()),
        Some(Duration::ZERO),
        "the next drag event arms it again"
    );
}

#[test]
fn a_held_drag_stops_firing_at_the_oldest_retained_line() {
    // The top-edge mirror: at the oldest line nothing more will ever appear
    // above, so a firing that moved nothing must not re-arm.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    for line_number in 0..60 {
        feed_terminal_output(
            &mut runtime_server,
            pane_id,
            format!("line{line_number}\r\n").as_bytes(),
        );
    }
    runtime_server.scroll_to_top(client_id, pane_id);

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 5);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let above = Point {
        column: start_point.column,
        row: get_pane_content_origin(&runtime_server, client_id, pane_id)
            .row
            .saturating_sub(3),
    };
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(above),
        clock.advance_one_second(),
    );
    let now = clock.advance_one_second();
    assert_eq!(
        viewer.next_mouse_wakeup(now),
        Some(Duration::ZERO),
        "the overshoot arms the scroll, and a second on it is overdue"
    );

    expire_mouse_scroll(
        &mut runtime_server,
        &mut viewer,
        now + Duration::from_millis(15),
    );
    assert_eq!(
        viewer.next_mouse_wakeup(now + Duration::from_millis(15)),
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
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"\x1b[?1049h"); // enter the alternate screen
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    let highlighted = get_selection(&mut runtime_server, client_id, pane_id).expect("highlighted");

    // The app scrolls on its own: cursor to the last row, a line feed there.
    feed_terminal_output(&mut runtime_server, pane_id, b"\x1b[999;1H\n");
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        Some(highlighted),
        "the highlight retained_selection until input into the pane clears it"
    );
}

#[test]
fn a_screen_highlight_survives_the_app_moving_rows_around() {
    // The app deleting or inserting lines moves screen rows, possibly leaving
    // the highlight over different text. Koshi leaves it alone — the app moved
    // the text, not koshi, and zellij behaves the same. The next click or key
    // into the pane clears it anyway, and copy captures the text at the drag's
    // release, so a moved highlight never corrupts a copy.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    let highlighted = get_selection(&mut runtime_server, client_id, pane_id).expect("highlighted");

    // DL with the cursor on row 3: rows below slide up, nothing is pushed.
    feed_terminal_output(&mut runtime_server, pane_id, b"\x1b[3;1H\x1b[M");
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        Some(highlighted),
        "the highlight retained_selection; what the app did to its rows is its business"
    );
}

#[test]
fn a_history_highlight_survives_a_primary_row_shift() {
    // History rows do not move when screen rows do — their numbers still name
    // the same text — so a highlight living entirely in history stands.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    for line_number in 0..60 {
        feed_terminal_output(
            &mut runtime_server,
            pane_id,
            format!("line{line_number}\r\n").as_bytes(),
        );
    }

    // Scrolled up three lines, the top three view rows are history rows.
    runtime_server.scroll_up(client_id, pane_id, 3);
    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 1);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    let highlighted = get_selection(&mut runtime_server, client_id, pane_id).expect("highlighted");

    feed_terminal_output(&mut runtime_server, pane_id, b"\x1b[10;1H\x1b[M");
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        Some(highlighted),
        "a highlight entirely in history is untouched by screen row moves"
    );
}

#[test]
fn a_highlight_on_the_alternate_screen_survives_a_redraw_in_place() {
    // Rewriting cells without moving rows — how a full-screen app updates a
    // status line — leaves the highlight standing, exactly as an in-place
    // redraw does on the primary screen.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"\x1b[?1049h");
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    let highlighted = get_selection(&mut runtime_server, client_id, pane_id).expect("highlighted");

    feed_terminal_output(&mut runtime_server, pane_id, b"\x1b[2;1Hredrawn text");
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        Some(highlighted),
        "no rows moved, so the highlight retained_selection"
    );
}

#[test]
fn a_plain_click_copies_nothing() {
    // A click's press highlights nothing, so its release has nothing to copy:
    // no clipboard write, and the clipboard the user already had is untouched.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    assert_eq!(runtime_server.take_host_writes(client_id), None);
}

#[test]
fn releasing_the_gesture_is_the_copy() {
    // No copy key exists: like zellij, releasing the selection IS the copy.
    // The highlighted text goes to the client's outer terminal as OSC 52 —
    // which sets the OS clipboard — and the highlight stays standing.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        runtime_server.take_host_writes(client_id),
        None,
        "nothing is copied while the drag is still moving"
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );

    // base64("hello") = aGVsbG8=
    assert_eq!(
        runtime_server
            .take_host_writes(client_id)
            .expect("queued clipboard write"),
        b"\x1b]52;c;aGVsbG8=\x07".to_vec()
    );
    let retained_selection = get_selection(&mut runtime_server, client_id, pane_id)
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
    let (mut runtime_server, viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut viewer = build_copying_viewer(
        &mut runtime_server,
        client_id,
        PartialCopyConfig {
            should_trim_trailing_whitespace: Some(false),
        },
    );
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"a");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_alt_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 2, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );

    assert_eq!(
        runtime_server
            .take_host_writes(client_id)
            .expect("queued clipboard write"),
        b"\x1b]52;c;YSAg\x07".to_vec()
    );
}

#[test]
fn ctrl_c_clears_the_highlight_like_any_key_reaching_the_pane() {
    // The exact chord a person presses to "copy": Ctrl+C. It is not bound, so
    // it falls through to the shell (SIGINT) — input reaching the pane's
    // child — and the highlight clears, exactly the behavior zellij shows.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    let highlighted = get_selection(&mut runtime_server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );

    // `<C-c>` binds nothing, so the viewer passes it through and the session
    // writes it to the pane.
    runtime_server.handle_key_input(
        client_id,
        &build_key_input_for_chord(KeyChord::from_parts(ModFlags::CTRL, Key::Char('c'))),
    );
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        None,
        "Ctrl+C reached the shell, so the highlight is gone"
    );
}

#[test]
fn typing_into_the_pane_clears_the_typists_highlight_there() {
    // The exit rule: input reaching the pane's child leaves visual mode. A key
    // no binding consumes is written to the child, so it clears the highlight,
    // the way typing replaces a selection in an editor.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    let highlighted = get_selection(&mut runtime_server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );

    runtime_server.handle_key_input(
        client_id,
        &build_key_input_for_chord(KeyChord::from_parts(ModFlags::NONE, Key::Char('x'))),
    );
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        None,
        "the key reached the child, so the highlight is gone"
    );
}

#[test]
fn typing_during_a_drag_cancels_the_highlight_and_the_gesture() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    let highlighted = get_selection(&mut runtime_server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );

    // The key reaches the pane's program, so the viewer drops the gesture and
    // the session drops the highlight — the way the binary's loop pairs them.
    viewer.end_mouse_selection();
    runtime_server.handle_key_input(
        client_id,
        &build_key_input_for_chord(KeyChord::from_parts(ModFlags::NONE, Key::Char('x'))),
    );
    assert_eq!(get_selection(&mut runtime_server, client_id, pane_id), None);

    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 6, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        None,
        "the drag has no gesture behind it, so it highlights nothing"
    );
}

#[test]
fn typing_after_a_press_cancels_the_empty_gesture() {
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    assert_eq!(get_selection(&mut runtime_server, client_id, pane_id), None);

    viewer.end_mouse_selection();
    runtime_server.handle_key_input(
        client_id,
        &build_key_input_for_chord(KeyChord::from_parts(ModFlags::NONE, Key::Char('x'))),
    );

    // The armed gesture is gone: a drag from here highlights nothing.
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    assert_eq!(get_selection(&mut runtime_server, client_id, pane_id), None);
}

#[test]
fn typing_leaves_another_panes_highlight_alone() {
    // Only the pane the key reaches ends selection activity; another pane's
    // highlight is not this key's business.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let end_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(end_point),
        clock.advance_one_second(),
    );
    let highlighted = get_selection(&mut runtime_server, client_id, pane_id).expect("highlighted");

    // The split focuses the new pane, so the key types into it.
    let other_pane_id = split_pane_rightward(&mut runtime_server, client_id);
    assert_ne!(other_pane_id, pane_id);
    runtime_server.handle_key_input(
        client_id,
        &build_key_input_for_chord(KeyChord::from_parts(ModFlags::NONE, Key::Char('x'))),
    );
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        Some(highlighted),
        "the highlight in the unfocused pane retained_selection"
    );
}

#[test]
fn a_click_forwarded_to_a_mouse_aware_program_clears_the_highlight() {
    // Same exit rule for the mouse: a click the program asked to see reaches
    // the child, so the highlight gets out of its way.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_release(screen_point),
        clock.advance_one_second(),
    );
    let highlighted = get_selection(&mut runtime_server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );

    // The program turns mouse reporting on; the next press is its, not koshi's.
    feed_terminal_output(&mut runtime_server, pane_id, b"\x1b[?1000h");
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        None,
        "the forwarded click reached the child, so the highlight is gone"
    );
}

#[test]
fn a_press_on_a_scrolled_view_highlights_the_history_line_the_user_saw() {
    // The frame says which line each pane's top visible row is, and the viewer
    // anchors on that. A view scrolled ten lines back must highlight the
    // history line under the pointer, not the live line that would sit at the
    // same screen row at the bottom.
    //
    // Sixty `lineNN` writes on a 20-row pane push 41 lines into history, so the
    // live view's top row is line 41 and a ten-line scroll back puts line 31
    // there. Output arriving between the paint and the press does not move it:
    // a scrolled view is held, so the text it shows stays put.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    for line_number in 0..60 {
        feed_terminal_output(
            &mut runtime_server,
            pane_id,
            format!("line{line_number:02}\r\n").as_bytes(),
        );
    }
    runtime_server.scroll_up(client_id, pane_id, 10);

    // The frame the viewer is looking at, taken before the output arrives.
    let painted_mouse_frame =
        MouseFrame::from(runtime_server.build_snapshot(client_id).expect("snapshot"));
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);

    // Eight more lines land while the pointer is on its way down.
    for line_number in 0..8 {
        feed_terminal_output(
            &mut runtime_server,
            pane_id,
            format!("late{line_number}\r\n").as_bytes(),
        );
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
            &mut runtime_server,
            &mut viewer,
            &painted_mouse_frame,
            mouse_actions,
        );
    }

    let copied = runtime_server
        .take_host_writes(client_id)
        .expect("the release copied");
    assert_eq!(
        copied,
        b"\x1b]52;c;bGluZTMx\x07".to_vec(),
        "the highlight is on `line31` — the line the mouse_frame's primary_history_line_count row showed — \
         and not on the live line that draws there when the view is at the \
         bottom"
    );
}

/// Envelope `command` as this client's mouse under `command_id` and run it, the
/// way a command arriving on the wire reaches the handler.
fn dispatch_mouse_command(
    runtime_server: &mut Server,
    client_id: ClientId,
    command_id: CommandId,
    command: Command,
) -> CommandResult {
    runtime_server.dispatch(CommandEnvelope::from_parts(
        command_id,
        CommandSource::from_mouse(client_id),
        SystemTime::now(),
        command,
    ))
}

/// How many rows `pane`'s content area has.
fn get_content_row_count(runtime_server: &Server, client_id: ClientId, pane_id: PaneId) -> u16 {
    let snapshot = runtime_server.build_snapshot(client_id).expect("snapshot");
    koshi_renderer::pane_content_rect(
        snapshot.build_frame_layout(ViewerChrome::default()),
        pane_id,
    )
    .expect("content rect")
    .cell_size
    .row_count
}

#[test]
fn copying_a_highlight_reaching_past_the_last_line_reads_the_lines_that_are_there() {
    // A highlight's row numbers arrive on the command wire and nothing between
    // there and the handler bounds them, so a command can name a row far past
    // anything the pane has ever held. The copy reads the rows the pane really
    // has and answers at once, rather than walking every number up to the one
    // named.
    let (mut runtime_server, viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let set = CommandId::new();
    let set_result = dispatch_mouse_command(
        &mut runtime_server,
        client_id,
        set,
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
    let stored =
        get_selection(&mut runtime_server, client_id, pane_id).expect("the highlight is stored");
    assert_eq!(
        set_result,
        CommandResult::Ok {
            command_id: set,
            emitted_events: vec![Event::SelectionChanged(SelectionChanged {
                client_id,
                pane_id,
                selection: Some(stored),
            })],
        }
    );

    let copy = CommandId::new();
    assert_eq!(
        dispatch_mouse_command(
            &mut runtime_server,
            client_id,
            copy,
            Command::Visual(VisualCommand::Copy(CopyArgs {
                pane_id,
                clipboard_target: CopyTarget::Osc52,
                should_trim_trailing_whitespace: true,
            })),
        ),
        CommandResult::Ok {
            command_id: copy,
            emitted_events: Vec::new(),
        },
        "a copy emits no event of its own"
    );

    // The pane holds one written row and blank rows under it; every row after
    // the first ends hard, so each contributes a newline and no text.
    let blank_rows = usize::from(get_content_row_count(&runtime_server, client_id, pane_id)) - 1;
    let expected_clipboard_text = format!("hello world{}", "\n".repeat(blank_rows));
    assert_eq!(
        runtime_server.take_host_writes(client_id),
        Some(crate::runtime::clipboard::osc52_copy(
            &expected_clipboard_text
        ))
    );
}

#[test]
fn a_copy_goes_to_the_clipboard_its_own_command_names() {
    // The viewer fills the target in from its own `copy.clipboard` setting, so
    // the session writes where the command says and never re-reads a setting of
    // its own. Koshi builds no native backend, so a copy naming one writes
    // nothing.
    let (mut runtime_server, viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello");

    let highlight = Selection {
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
    let set = CommandId::new();
    assert_eq!(
        dispatch_mouse_command(
            &mut runtime_server,
            client_id,
            set,
            Command::Visual(VisualCommand::SetSelection(SetSelectionArgs {
                pane_id,
                selection: highlight,
            })),
        ),
        CommandResult::Ok {
            command_id: set,
            emitted_events: vec![Event::SelectionChanged(SelectionChanged {
                client_id,
                pane_id,
                selection: Some(highlight),
            })],
        }
    );

    let to_native = CommandId::new();
    assert_eq!(
        dispatch_mouse_command(
            &mut runtime_server,
            client_id,
            to_native,
            Command::Visual(VisualCommand::Copy(CopyArgs {
                pane_id,
                clipboard_target: CopyTarget::Native,
                should_trim_trailing_whitespace: true,
            })),
        ),
        CommandResult::Ok {
            command_id: to_native,
            emitted_events: Vec::new(),
        }
    );
    assert_eq!(
        runtime_server.take_host_writes(client_id),
        None,
        "a copy to the native clipboard queues no escape for the outer terminal"
    );

    let to_osc52 = CommandId::new();
    assert_eq!(
        dispatch_mouse_command(
            &mut runtime_server,
            client_id,
            to_osc52,
            Command::Visual(VisualCommand::Copy(CopyArgs {
                pane_id,
                clipboard_target: CopyTarget::Osc52,
                should_trim_trailing_whitespace: true,
            })),
        ),
        CommandResult::Ok {
            command_id: to_osc52,
            emitted_events: Vec::new(),
        }
    );
    // base64("hello") = aGVsbG8=
    assert_eq!(
        runtime_server.take_host_writes(client_id),
        Some(b"\x1b]52;c;aGVsbG8=\x07".to_vec()),
        "the same highlight copied to OSC 52 reaches the outer terminal"
    );
}

#[test]
fn a_drag_ends_when_its_pane_swaps_to_the_alternate_screen() {
    // The drag's anchor names a line of the primary screen's text. Once the
    // program is on the alternate screen those lines are not on show, so
    // extending from that anchor would highlight text the user never pointed
    // at. The gesture ends with the swap and the next motion asks for nothing.
    let (mut runtime_server, mut viewer, pane_id) = build_selection_runtime();
    let client_id = viewer.get_client_id();
    let mut clock = Clock::new();
    feed_terminal_output(&mut runtime_server, pane_id, b"hello world");

    let start_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 0, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_press(start_point),
        clock.advance_one_second(),
    );
    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 4, 0);
    process_mouse_input(
        &mut runtime_server,
        &mut viewer,
        build_mouse_drag(screen_point),
        clock.advance_one_second(),
    );
    let highlighted = get_selection(&mut runtime_server, client_id, pane_id).expect("highlighted");
    assert_eq!(
        highlighted.anchor,
        GridPosition {
            row_index: 0,
            column_index: 0
        }
    );
    assert_eq!(
        highlighted.cursor,
        GridPosition {
            row_index: 0,
            column_index: 4
        }
    );

    // The program enters the alternate screen mid-drag (what `vim` does on
    // start); the session drops the highlight it made.
    feed_terminal_output(&mut runtime_server, pane_id, b"\x1b[?1049h");

    let screen_point = get_pane_screen_cell(&runtime_server, client_id, pane_id, 8, 0);
    let mouse_frame = MouseFrame::from(runtime_server.build_snapshot(client_id).expect("snapshot"));
    let mouse_actions = viewer.handle_mouse(
        build_mouse_drag(screen_point),
        &mouse_frame,
        clock.advance_one_second(),
    );

    assert_eq!(
        mouse_actions,
        Vec::new(),
        "no drag is under way, so the motion asks for no highlight"
    );
    apply_mouse_actions(
        &mut runtime_server,
        &mut viewer,
        &mouse_frame,
        mouse_actions,
    );
    assert_eq!(
        get_selection(&mut runtime_server, client_id, pane_id),
        None,
        "and nothing put a highlight back on the alternate screen"
    );
}
