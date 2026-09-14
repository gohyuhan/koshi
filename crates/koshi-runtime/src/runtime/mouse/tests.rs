//! Mouse routing tests, driven through both halves: the viewer answers each
//! event from the mouse_frame it painted and the session executes what came back,
//! exactly as the running binary does.
//!
//! Session state is read back through [`Server::build_snapshot`] — the same
//! projection the renderer draws — and viewer state through
//! [`ViewerClient::chrome`], so a test never reaches into private fields of
//! either half.

use super::*;

use std::collections::VecDeque;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use koshi_client::mouse::{MouseAction, TABLINE_DRAG_CELL_COUNT};
use koshi_client::Client as ViewerClient;
use koshi_config::layer::{PartialKoshiConfig, PartialMouseConfig};
use koshi_config::types::WheelScroll;
use koshi_core::command::{
    FocusTabArgs, GridPosition, NewPaneArgs, NewTabArgs, Selection, SelectionKind, TabTarget,
};
use koshi_core::geometry::{Direction, PaneArea, Point, Size};
use koshi_core::ids::SessionId;
use koshi_core::key::ModFlags;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind, ScrollDirection};
use koshi_layout::mode::LayoutMode;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_pty::error::PtyError;
use koshi_renderer::snapshot::{Delivery, MouseFrame, ViewerChrome};
use koshi_renderer::{compute_pane_local_cell, hit_test, HitRegion};
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::runtime::bus::EventFilter;

fn build_runtime() -> (Server, ClientId) {
    let (runtime, _fake_pty_backend, client_id) = build_runtime_with_fake_pty_backend();
    (runtime, client_id)
}

/// A `new-pane` request with nothing chosen: the focused pane splits rightward.
fn build_new_pane_args() -> NewPaneArgs {
    NewPaneArgs {
        source_pane_id: None,
        tab_id: None,
        direction: Direction::Right,
        should_stack: false,
        working_directory: None,
        spawn_spec: None,
        client_id: None,
    }
}

/// The viewer half for `client_id`, on the stock settings: it holds the `mouse`
/// and `copy` config and answers every mouse event below before the session
/// hears about it.
fn build_viewer(runtime: &mut Server, client_id: ClientId) -> ViewerClient {
    ViewerClient::from_client_id_and_viewport(
        client_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
        runtime.subscribe(client_id, EventFilter::All),
        TerminalCleanupGuard::new(),
    )
}

/// The same viewer, with its `mouse.wheel` setting on `wheel_scroll`.
fn build_viewer_with_wheel(
    runtime: &mut Server,
    client_id: ClientId,
    wheel_scroll: WheelScroll,
) -> ViewerClient {
    let mut viewer = build_viewer(runtime, client_id);
    viewer.load_startup_config(
        Some(PartialKoshiConfig {
            mouse: Some(PartialMouseConfig {
                wheel: Some(wheel_scroll),
                ..PartialMouseConfig::default()
            }),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );
    viewer
}

/// One mouse event, the way the running binary delivers it: the viewer decides
/// what it means against the mouse_frame it is looking position, and only what it decided
/// reaches the session.
///
/// Timed far enough from any other that no two presses read as a double click —
/// the runtime tells a double click from two separate clicks by the gap between
/// them, so a test that pressed twice at the wall clock would double-click by
/// accident. A test that wants a real double click drives [`dispatch_mouse_input_at_time`] with its
/// own instants.
fn dispatch_mouse_input(runtime: &mut Server, viewer: &mut ViewerClient, mouse_input: MouseInput) {
    dispatch_mouse_input_at_time(runtime, viewer, mouse_input, compute_separated_event_time());
}

/// [`dispatch_mouse_input`] with the instant the event happened position, for the tests that drive
/// the click threshold themselves.
fn dispatch_mouse_input_at_time(
    runtime: &mut Server,
    viewer: &mut ViewerClient,
    mouse_input: MouseInput,
    event_time: Instant,
) {
    viewer.apply_events();
    let mouse_frame = MouseFrame::from(
        runtime
            .build_snapshot(viewer.get_client_id())
            .expect("render snapshot"),
    );
    let mouse_actions = viewer.handle_mouse(mouse_input, &mouse_frame, event_time);
    apply_mouse_actions(runtime, viewer, &mouse_frame, mouse_actions);
}

/// Run everything the viewer decided, the way the binary's loop does.
fn apply_mouse_actions(
    runtime: &mut Server,
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
                let view_top_row_index = runtime.scroll_pane_view(
                    client_id,
                    pane_id,
                    is_scrolling_up,
                    scroll_line_count,
                );
                mouse_action_queue.extend(viewer.note_scroll_applied(
                    pane_id,
                    view_top_row_index,
                    mouse_frame,
                ));
            }
            MouseAction::Forward {
                pane_id,
                mouse_input,
            } => {
                let was_written = runtime.forward_mouse_to_pane(client_id, pane_id, mouse_input);
                if let (true, MouseKind::Press(mouse_button)) =
                    (was_written, mouse_input.mouse_kind)
                {
                    viewer.note_press_forwarded(pane_id, mouse_button);
                }
            }
            MouseAction::AltScrollArrows {
                pane_id,
                is_scrolling_up,
                arrow_count,
            } => {
                runtime.write_alt_scroll_arrows(pane_id, is_scrolling_up, arrow_count);
            }
            MouseAction::Resize {
                pane_id,
                border_side,
                resize_step,
                requested_cell_count,
            } => {
                let applied_cell_count = runtime.drag_resize(
                    client_id,
                    pane_id,
                    border_side,
                    resize_step,
                    requested_cell_count,
                );
                viewer.note_resize_applied(pane_id, border_side, resize_step, applied_cell_count);
            }
            MouseAction::Command(command) => {
                let command_envelope = CommandEnvelope::from_parts(
                    CommandId::new(),
                    CommandSource::from_mouse(client_id),
                    SystemTime::now(),
                    command,
                );
                let _ = runtime.submit_command(command_envelope);
            }
        }
    }
}

/// An instant an hour after the last one this returned, so successive presses
/// never fall inside a click threshold.
fn compute_separated_event_time() -> Instant {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ELAPSED_HOUR_COUNT: AtomicU64 = AtomicU64::new(1);
    let elapsed_hour_count = ELAPSED_HOUR_COUNT.fetch_add(1, Ordering::Relaxed);
    Instant::now() + Duration::from_secs(elapsed_hour_count * 3600)
}

fn build_runtime_with_fake_pty_backend() -> (Server, Arc<FakePtyBackend>, ClientId) {
    build_sized_runtime(Size {
        column_count: 80,
        row_count: 24,
    })
}

/// [`build_runtime_with_fake_pty_backend`] on a viewport of `viewport_size`, for a case that needs
/// room for more panes than the stock 80 by 24 holds.
fn build_sized_runtime(viewport_size: Size) -> (Server, Arc<FakePtyBackend>, ClientId) {
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (tx, rx) = mpsc::channel();
    let mut runtime = Server::from_runtime_parts(fake_pty_backend.clone(), rx, tx);
    let client_id = runtime
        .bootstrap_local(SessionId::new(), viewport_size, SystemTime::UNIX_EPOCH)
        .expect("bootstrap client");
    (runtime, fake_pty_backend, client_id)
}

/// The active tab's panes top to bottom, each with the row count the layout
/// solved for it.
fn list_stacked_pane_sizes(runtime: &Server, client_id: ClientId) -> Vec<(PaneId, u16)> {
    let render_snapshot = runtime.build_snapshot(client_id).expect("render snapshot");
    let mut stacked_pane_positions: Vec<(u16, PaneId, u16)> = render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| {
            (
                pane_slot.outer_rect.origin.row,
                pane_slot.pane_id,
                pane_slot.outer_rect.cell_size.row_count,
            )
        })
        .collect();
    stacked_pane_positions.sort_unstable();
    stacked_pane_positions
        .into_iter()
        .map(|(_, pane_id, row_count)| (pane_id, row_count))
        .collect()
}

/// List the pane row counts from [`list_stacked_pane_sizes`].
fn list_pane_row_counts(pane_sizes: &[(PaneId, u16)]) -> Vec<u16> {
    pane_sizes.iter().map(|&(_, row_count)| row_count).collect()
}

/// The client's single bootstrap pane.
fn get_only_pane_id(runtime: &Server) -> PaneId {
    *runtime
        .pty_handle_by_pane_id
        .keys()
        .next()
        .expect("one pane")
}

/// A screen cell inside `pane_id`'s content, with the 1-based pane-local column and
/// row a mouse report would carry for it.
fn find_pane_content_cell(
    runtime: &Server,
    client_id: ClientId,
    pane_id: PaneId,
) -> (Point, u16, u16) {
    let render_snapshot = runtime.build_snapshot(client_id).expect("render snapshot");
    let viewport_size = render_snapshot.client_snapshot.viewport_size;
    for row_index in 0..viewport_size.row_count {
        for column_index in 0..viewport_size.column_count {
            let screen_point = Point {
                column: column_index,
                row: row_index,
            };
            if hit_test(
                render_snapshot.build_frame_layout(ViewerChrome::default()),
                screen_point,
            ) == (HitRegion::PaneContent { pane_id })
            {
                let (pane_column, pane_row) = compute_pane_local_cell(
                    render_snapshot.build_frame_layout(ViewerChrome::default()),
                    pane_id,
                    screen_point,
                )
                .expect("local cell");
                return (screen_point, pane_column, pane_row);
            }
        }
    }
    panic!("no content cell for the pane");
}

fn build_left_press_input(column_index: u16, row_index: u16) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Press(MouseButton::Left),
        position: Point {
            column: column_index,
            row: row_index,
        },
        modifier_flags: ModFlags::NONE,
    }
}

fn build_tab(runtime: &mut Server, client_id: ClientId) {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        SystemTime::now(),
        Command::NewTab(NewTabArgs::default()),
    );
    let _ = runtime.dispatch(command_envelope);
}

/// The first cell on the tabline row whose hit region satisfies
/// `region_predicate`, scanning from `minimum_column`.
fn find_tabline_pointer_column(
    runtime: &Server,
    viewer: &ViewerClient,
    minimum_column: u16,
    region_predicate: impl Fn(HitRegion) -> bool,
) -> u16 {
    let render_snapshot = runtime
        .build_snapshot(viewer.get_client_id())
        .expect("render snapshot");
    let viewer_chrome = viewer.build_viewer_chrome(render_snapshot.client_snapshot.active_tab_id);
    (minimum_column..render_snapshot.client_snapshot.viewport_size.column_count)
        .find(|&pointer_column| {
            region_predicate(hit_test(
                render_snapshot.build_frame_layout(viewer_chrome),
                Point {
                    column: pointer_column,
                    row: 0,
                },
            ))
        })
        .expect("a matching tabline cell")
}

/// Where the viewer's tab strip is scrolled to, for the tab it is showing.
fn get_tabline_offset(runtime: &Server, viewer: &ViewerClient) -> Option<usize> {
    let render_snapshot = runtime
        .build_snapshot(viewer.get_client_id())
        .expect("render snapshot");
    viewer
        .build_viewer_chrome(render_snapshot.client_snapshot.active_tab_id)
        .tabline_offset
}

/// Scroll the viewer's tab strip to `target_tab_index` by wheeling over the
/// strip, the only way a viewer's tabline offset moves.
fn scroll_tabline_to_index(
    runtime: &mut Server,
    viewer: &mut ViewerClient,
    target_tab_index: usize,
) {
    // Each wheel_input steps one tab; walking down from the far end lands on any index
    // whatever the strip was showing.
    let tab_count = runtime
        .build_snapshot(viewer.get_client_id())
        .expect("render snapshot")
        .session_snapshot
        .tabs_metadata
        .len();
    for _ in 0..tab_count {
        dispatch_mouse_input(
            runtime,
            viewer,
            build_wheel_input(ScrollDirection::Up, Point { column: 0, row: 0 }),
        );
    }
    for _ in 0..target_tab_index {
        dispatch_mouse_input(
            runtime,
            viewer,
            build_wheel_input(ScrollDirection::Down, Point { column: 0, row: 0 }),
        );
    }
    assert_eq!(
        get_tabline_offset(runtime, viewer),
        Some(target_tab_index),
        "the peek was set up"
    );
}

#[test]
fn clicking_an_inactive_tab_focuses_it_and_clears_the_peek() {
    let (mut runtime, client_id) = build_runtime();
    for _ in 0..30 {
        build_tab(&mut runtime, client_id); // overflow the 80-column strip
    }
    let mut viewer = build_viewer(&mut runtime, client_id);

    // Peek from tab 0 so it is on the strip regardless of how wide the
    // auto-generated session and tab names happen to render, then click that
    // (now inactive) tab.
    scroll_tabline_to_index(&mut runtime, &mut viewer, 0);
    let render_snapshot = runtime.build_snapshot(client_id).expect("render snapshot");
    let first_tab_id = render_snapshot
        .session_snapshot
        .tabs_metadata
        .iter()
        .find(|tab_meta| tab_meta.tab_index == 0)
        .expect("a first tab")
        .tab_id;
    let pointer_column = find_tabline_pointer_column(&runtime, &viewer, 0, |region| {
        region
            == HitRegion::Tab {
                tab_id: first_tab_id,
            }
    });

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(pointer_column, 0),
    );

    let render_snapshot = runtime.build_snapshot(client_id).expect("render snapshot");
    assert_eq!(
        render_snapshot.client_snapshot.active_tab_id, first_tab_id,
        "clicked tab is active"
    );
    assert_eq!(
        get_tabline_offset(&runtime, &viewer),
        None,
        "peek cleared on switch"
    );
}

#[test]
fn a_tab_switch_by_any_route_reveals_the_new_tab() {
    // The peek belongs to the tab it was made on, so a switch driven from
    // anywhere — here a `focus-tab` command, not a click — reveals the new tab.
    let (mut runtime, client_id) = build_runtime();
    for _ in 0..30 {
        build_tab(&mut runtime, client_id);
    }
    let mut viewer = build_viewer(&mut runtime, client_id);
    scroll_tabline_to_index(&mut runtime, &mut viewer, 3);

    let first_tab_id = runtime
        .build_snapshot(client_id)
        .expect("render snapshot")
        .session_snapshot
        .tabs_metadata
        .iter()
        .find(|tab_meta| tab_meta.tab_index == 0)
        .expect("a first tab")
        .tab_id;
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        SystemTime::now(),
        Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Id(first_tab_id),
            client_id: Some(client_id),
        }),
    );
    let _ = runtime.dispatch(command_envelope);

    assert_eq!(
        get_tabline_offset(&runtime, &viewer),
        None,
        "the peek did not survive"
    );
}

#[test]
fn clicking_the_right_scroll_arrow_peeks_toward_the_end() {
    let (mut runtime, client_id) = build_runtime();
    for _ in 0..30 {
        build_tab(&mut runtime, client_id); // overflow the 80-column strip
    }
    let mut viewer = build_viewer(&mut runtime, client_id);
    scroll_tabline_to_index(&mut runtime, &mut viewer, 0);

    let pointer_column = find_tabline_pointer_column(&runtime, &viewer, 0, |region| {
        matches!(region, HitRegion::TablineScrollRight { .. })
    });
    let render_snapshot = runtime.build_snapshot(client_id).expect("render snapshot");
    let viewer_chrome = viewer.build_viewer_chrome(render_snapshot.client_snapshot.active_tab_id);
    let target_tab_index = match hit_test(
        render_snapshot.build_frame_layout(viewer_chrome),
        Point {
            column: pointer_column,
            row: 0,
        },
    ) {
        HitRegion::TablineScrollRight { target_tab_index } => target_tab_index,
        unexpected_hit_region => {
            panic!("expected a right scroll arrow, got {unexpected_hit_region:?}")
        }
    };

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(pointer_column, 0),
    );

    assert!(
        target_tab_index > 0,
        "the right arrow scrolls toward the end"
    );
    assert_eq!(
        get_tabline_offset(&runtime, &viewer),
        Some(target_tab_index)
    );
}

#[test]
fn wheel_over_the_tabline_steps_the_offset() {
    let (mut runtime, client_id) = build_runtime();
    for _ in 0..30 {
        build_tab(&mut runtime, client_id);
    }
    let mut viewer = build_viewer(&mut runtime, client_id);
    scroll_tabline_to_index(&mut runtime, &mut viewer, 0);

    let pointer_column = find_tabline_pointer_column(&runtime, &viewer, 0, |region| {
        matches!(
            region,
            HitRegion::Tab { .. } | HitRegion::TablineScrollRight { .. }
        )
    });

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(
            ScrollDirection::Down,
            Point {
                column: pointer_column,
                row: 0,
            },
        ),
    );

    assert_eq!(
        get_tabline_offset(&runtime, &viewer),
        Some(1),
        "wheel down steps one tab"
    );
}

#[test]
fn a_wheel_off_the_tabline_row_does_not_scroll_it() {
    let (mut runtime, client_id) = build_runtime();
    for _ in 0..30 {
        build_tab(&mut runtime, client_id);
    }
    let mut viewer = build_viewer(&mut runtime, client_id);
    scroll_tabline_to_index(&mut runtime, &mut viewer, 2);

    // Row 10 is pane content, not the tabline.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(
            ScrollDirection::Down,
            Point {
                column: 40,
                row: 10,
            },
        ),
    );

    assert_eq!(
        get_tabline_offset(&runtime, &viewer),
        Some(2),
        "offset unchanged off-row"
    );
}

#[test]
fn motion_and_non_left_buttons_leave_state_untouched() {
    let (mut runtime, client_id) = build_runtime();
    for _ in 0..30 {
        build_tab(&mut runtime, client_id);
    }
    let mut viewer = build_viewer(&mut runtime, client_id);
    scroll_tabline_to_index(&mut runtime, &mut viewer, 2);

    // Buttonless motion over the tabline scrolls nothing and begins no drag.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        MouseInput {
            mouse_kind: MouseKind::Motion,
            position: Point { column: 5, row: 0 },
            modifier_flags: ModFlags::NONE,
        },
    );
    // A right press over a tab is neither a focus nor a scroll.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        MouseInput {
            mouse_kind: MouseKind::Press(MouseButton::Right),
            position: Point { column: 5, row: 0 },
            modifier_flags: ModFlags::NONE,
        },
    );

    assert_eq!(
        get_tabline_offset(&runtime, &viewer),
        Some(2),
        "ignored events do not scroll"
    );

    // No drag began: a left drag now scrolls nothing either.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(5 + TABLINE_DRAG_CELL_COUNT as u16, 0),
    );
    assert_eq!(
        get_tabline_offset(&runtime, &viewer),
        Some(2),
        "ignored events begin no drag"
    );
}

#[test]
fn dragging_scrolls_from_the_anchor_and_release_ends_it() {
    let (mut runtime, client_id) = build_runtime();
    for _ in 0..30 {
        build_tab(&mut runtime, client_id);
    }
    let mut viewer = build_viewer(&mut runtime, client_id);
    scroll_tabline_to_index(&mut runtime, &mut viewer, 2);

    // Press a bare tabline cell far enough along the row that a two-step drag
    // to its left stays on screen.
    let anchor_column = find_tabline_pointer_column(
        &runtime,
        &viewer,
        2 * TABLINE_DRAG_CELL_COUNT as u16,
        |region| region == HitRegion::Tabline,
    );
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(anchor_column, 0),
    );

    // Drag left by two steps' worth of cells: scroll two count_tabs toward the end.
    let pointer_column = anchor_column - 2 * TABLINE_DRAG_CELL_COUNT as u16;
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(pointer_column, 0),
    );
    assert_eq!(
        get_tabline_offset(&runtime, &viewer),
        Some(4),
        "two steps past anchor 2"
    );

    // Release ends the drag, leaving the scrolled offset.
    dispatch_mouse_input(&mut runtime, &mut viewer, build_left_release_input());
    assert_eq!(
        get_tabline_offset(&runtime, &viewer),
        Some(4),
        "offset stays after release"
    );

    // A drag after release with no press behind it scrolls nothing.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(anchor_column + 2 * TABLINE_DRAG_CELL_COUNT as u16, 0),
    );
    assert_eq!(
        get_tabline_offset(&runtime, &viewer),
        Some(4),
        "release ended the drag"
    );
}

fn build_left_drag_input(column_index: u16, row_index: u16) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Drag(MouseButton::Left),
        position: Point {
            column: column_index,
            row: row_index,
        },
        modifier_flags: ModFlags::NONE,
    }
}

fn build_left_release_input() -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Release(MouseButton::Left),
        position: Point { column: 0, row: 0 },
        modifier_flags: ModFlags::NONE,
    }
}

/// Split the focused pane in the runtime's default direction (Right), leaving
/// the tab with two side-by-side panes and a vertical border between them.
fn split_focused_pane(runtime: &mut Server, client_id: ClientId) {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        SystemTime::now(),
        Command::NewPane(build_new_pane_args()),
    );
    let _ = runtime.dispatch(command_envelope);
}

/// The solved width, in columns, of `pane`'s box in `client`'s current mouse_frame.
fn pane_column_count(runtime: &Server, client_id: ClientId, pane_id: PaneId) -> u16 {
    let render_snapshot = runtime.build_snapshot(client_id).expect("render snapshot");
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .find(|pane_slot| pane_slot.pane_id == pane_id)
        .expect("pane in layout")
        .outer_rect
        .cell_size
        .column_count
}

/// A cell on the vertical divider between two side-by-side panes: the left/right
/// border nearest the horizontal center, so it is the shared divider rather than
/// the pane area's outer mouse_frame at either edge. Panics if the mouse_frame has no
/// vertical border.
fn find_vertical_border(runtime: &Server, client_id: ClientId) -> (Point, PaneId, Direction) {
    let render_snapshot = runtime.build_snapshot(client_id).expect("render snapshot");
    let viewport_size = render_snapshot.client_snapshot.viewport_size;
    let row_index = viewport_size.row_count / 2;
    let center_column = viewport_size.column_count / 2;
    let mut nearest_border: Option<(u16, PaneId, Direction)> = None;
    for column_index in 0..viewport_size.column_count {
        if let HitRegion::PaneBorder {
            pane_id,
            side: border_side,
        } = hit_test(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            Point {
                column: column_index,
                row: row_index,
            },
        ) {
            if matches!(border_side, Direction::Left | Direction::Right)
                && nearest_border.is_none_or(|(border_column, ..)| {
                    center_column.abs_diff(column_index) < center_column.abs_diff(border_column)
                })
            {
                nearest_border = Some((column_index, pane_id, border_side));
            }
        }
    }
    let (border_column, pane_id, border_side) =
        nearest_border.expect("a vertical pane border in the mouse_frame");
    (
        Point {
            column: border_column,
            row: row_index,
        },
        pane_id,
        border_side,
    )
}

/// Compute the column `cell_count` cells outward from `border_column` for a
/// vertical `border_side`.
fn compute_outward_column(border_side: Direction, border_column: u16, cell_count: u16) -> u16 {
    match border_side {
        Direction::Right => border_column + cell_count,
        Direction::Left => border_column - cell_count,
        unexpected_direction => panic!("expected a vertical border, got {unexpected_direction:?}"),
    }
}

/// Compute the column `cell_count` cells inward from `border_column` for a
/// vertical `border_side`.
fn compute_inward_column(border_side: Direction, border_column: u16, cell_count: u16) -> u16 {
    match border_side {
        Direction::Right => border_column - cell_count,
        Direction::Left => border_column + cell_count,
        unexpected_direction => panic!("expected a vertical border, got {unexpected_direction:?}"),
    }
}

/// The far viewport edge on a border's outward side: a drag there grows the
/// grabbed pane by more than its neighbor can ever donate.
fn compute_outward_edge_column(border_side: Direction, viewport_column_count: u16) -> u16 {
    match border_side {
        Direction::Right => viewport_column_count - 1,
        Direction::Left => 0,
        unexpected_direction => panic!("expected a vertical border, got {unexpected_direction:?}"),
    }
}

#[test]
fn dragging_a_vertical_border_resizes_the_grabbed_pane_live() {
    let (mut runtime, client) = build_runtime();
    let mut viewer = build_viewer(&mut runtime, client);
    split_focused_pane(&mut runtime, client);

    let (border_cell, pane_id, border_side) = find_vertical_border(&runtime, client);
    let initial_column_count = pane_column_count(&runtime, client, pane_id);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(border_cell.column, border_cell.row),
    );
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(
            compute_outward_column(border_side, border_cell.column, 3),
            border_cell.row,
        ),
    );

    assert_eq!(
        pane_column_count(&runtime, client, pane_id),
        initial_column_count + 3,
        "the grabbed pane grew by the three cells dragged toward its border"
    );
}

#[test]
fn a_shrink_drag_tracks_the_pointer_cell_for_cell() {
    let (mut runtime, client) = build_runtime();
    let mut viewer = build_viewer(&mut runtime, client);
    split_focused_pane(&mut runtime, client);

    let (border_cell, pane_id, border_side) = find_vertical_border(&runtime, client);
    let initial_column_count = pane_column_count(&runtime, client, pane_id);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(border_cell.column, border_cell.row),
    );

    // Drag three cells inward to shrink the pane.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(
            compute_inward_column(border_side, border_cell.column, 3),
            border_cell.row,
        ),
    );
    assert_eq!(
        pane_column_count(&runtime, client, pane_id),
        initial_column_count - 3,
        "the grabbed pane shrank by the three cells dragged inward"
    );

    // One more cell inward from the new pointer position shrinks by exactly one
    // more: the anchor followed the pointer, so it is not a sudden jump.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(
            compute_inward_column(border_side, border_cell.column, 4),
            border_cell.row,
        ),
    );
    assert_eq!(
        pane_column_count(&runtime, client, pane_id),
        initial_column_count - 4,
        "the second drag shrinks one cell, tracking the pointer"
    );
}

#[test]
fn a_release_ends_the_resize_drag_so_a_new_drag_does_nothing() {
    let (mut runtime, client) = build_runtime();
    let mut viewer = build_viewer(&mut runtime, client);
    split_focused_pane(&mut runtime, client);

    let (border_cell, pane_id, border_side) = find_vertical_border(&runtime, client);
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(border_cell.column, border_cell.row),
    );
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(
            compute_outward_column(border_side, border_cell.column, 2),
            border_cell.row,
        ),
    );
    let column_count_after_drag = pane_column_count(&runtime, client, pane_id);

    dispatch_mouse_input(&mut runtime, &mut viewer, build_left_release_input());

    // With no resize drag in progress, a stray drag resizes nothing.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(
            compute_outward_column(border_side, border_cell.column, 6),
            border_cell.row,
        ),
    );
    assert_eq!(
        pane_column_count(&runtime, client, pane_id),
        column_count_after_drag,
        "no resize drag is in progress, so the pointer move is ignored"
    );
}

#[test]
fn a_fast_over_drag_fills_to_the_wall_then_reverses_at_once() {
    let (mut runtime, client) = build_runtime();
    let mut viewer = build_viewer(&mut runtime, client);
    split_focused_pane(&mut runtime, client);

    let (border_cell, pane_id, border_side) = find_vertical_border(&runtime, client);
    let initial_column_count = pane_column_count(&runtime, client, pane_id);
    let viewport_column_count = runtime
        .build_snapshot(client)
        .unwrap()
        .client_snapshot
        .viewport_size
        .column_count;

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(border_cell.column, border_cell.row),
    );

    // One big jump past the wall: the drag is applied a cell at a time, so it
    // grows the pane as far as the neighbor can donate instead of refusing the
    // whole move.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(
            compute_outward_edge_column(border_side, viewport_column_count),
            border_cell.row,
        ),
    );
    let grown_column_count = pane_column_count(&runtime, client, pane_id);
    assert_eq!(
        initial_column_count, 40,
        "the split starts even, 40 columns each"
    );
    assert_eq!(
        grown_column_count, 76,
        "the jump grew the pane by every column the neighbor could donate"
    );

    // Pointer still further out: the neighbor is already at its minimum, so the
    // anchor sits at the wall and nothing more moves.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(
            compute_outward_edge_column(border_side, viewport_column_count),
            border_cell.row,
        ),
    );
    assert_eq!(
        pane_column_count(&runtime, client, pane_id),
        grown_column_count,
        "held at the wall while the pointer overshoots"
    );

    // Reverse straight back to the original border cell: the anchor held at the
    // wall, so the pane shrinks back with no dead zone.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(border_cell.column, border_cell.row),
    );
    assert_eq!(
        pane_column_count(&runtime, client, pane_id),
        initial_column_count,
        "a reverse drag returns the border to where it started, no lag"
    );
}

/// Split the focused pane downward, leaving the tab with a top and bottom pane
/// and a horizontal border between them.
fn split_focused_downward(runtime: &mut Server, client_id: ClientId) {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        SystemTime::now(),
        Command::NewPane(NewPaneArgs {
            direction: Direction::Down,
            ..build_new_pane_args()
        }),
    );
    let _ = runtime.dispatch(command_envelope);
}

/// The solved row count of `pane_id`'s box in `client_id`'s current render
/// render_snapshot.
fn pane_row_count(runtime: &Server, client_id: ClientId, pane_id: PaneId) -> u16 {
    let render_snapshot = runtime.build_snapshot(client_id).expect("render snapshot");
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .find(|pane_slot| pane_slot.pane_id == pane_id)
        .expect("pane in layout")
        .outer_rect
        .cell_size
        .row_count
}

/// A cell on the horizontal divider between a top and bottom pane: the up/down
/// border nearest the vertical center, so it is the shared divider rather than
/// the outer mouse_frame. Panics if the mouse_frame has no horizontal border.
fn find_horizontal_border(runtime: &Server, client_id: ClientId) -> (Point, PaneId, Direction) {
    let render_snapshot = runtime.build_snapshot(client_id).expect("render snapshot");
    let viewport_size = render_snapshot.client_snapshot.viewport_size;
    let column_index = viewport_size.column_count / 2;
    let center_row = viewport_size.row_count / 2;
    let mut nearest_border: Option<(u16, PaneId, Direction)> = None;
    for row_index in 1..viewport_size.row_count - 1 {
        if let HitRegion::PaneBorder {
            pane_id,
            side: border_side,
        } = hit_test(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            Point {
                column: column_index,
                row: row_index,
            },
        ) {
            if matches!(border_side, Direction::Up | Direction::Down)
                && nearest_border.is_none_or(|(border_row, ..)| {
                    center_row.abs_diff(row_index) < center_row.abs_diff(border_row)
                })
            {
                nearest_border = Some((row_index, pane_id, border_side));
            }
        }
    }
    let (border_row, pane_id, border_side) =
        nearest_border.expect("a horizontal pane border in the mouse_frame");
    (
        Point {
            column: column_index,
            row: border_row,
        },
        pane_id,
        border_side,
    )
}

/// Compute the row `cell_count` cells outward from `border_row` for a horizontal
/// `border_side`.
fn compute_outward_row(border_side: Direction, border_row: u16, cell_count: u16) -> u16 {
    match border_side {
        Direction::Down => border_row + cell_count,
        Direction::Up => border_row - cell_count,
        unexpected_direction => {
            panic!("expected a horizontal border, got {unexpected_direction:?}")
        }
    }
}

/// The rightmost vertical border in the mouse_frame: the pane area's outer right
/// mouse_frame, which has no neighbor on its outward side.
fn find_outer_vertical_frame(runtime: &Server, client_id: ClientId) -> (Point, PaneId, Direction) {
    let render_snapshot = runtime.build_snapshot(client_id).expect("render snapshot");
    let viewport_size = render_snapshot.client_snapshot.viewport_size;
    let row_index = viewport_size.row_count / 2;
    let mut rightmost_border: Option<(u16, PaneId, Direction)> = None;
    for column_index in 0..viewport_size.column_count {
        if let HitRegion::PaneBorder {
            pane_id,
            side: border_side,
        } = hit_test(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            Point {
                column: column_index,
                row: row_index,
            },
        ) {
            if matches!(border_side, Direction::Left | Direction::Right)
                && rightmost_border.is_none_or(|(border_column, ..)| column_index > border_column)
            {
                rightmost_border = Some((column_index, pane_id, border_side));
            }
        }
    }
    let (border_column, pane_id, border_side) =
        rightmost_border.expect("a vertical pane border in the mouse_frame");
    (
        Point {
            column: border_column,
            row: row_index,
        },
        pane_id,
        border_side,
    )
}

#[test]
fn dragging_a_horizontal_border_resizes_the_grabbed_pane_live() {
    let (mut runtime, client) = build_runtime();
    let mut viewer = build_viewer(&mut runtime, client);
    split_focused_downward(&mut runtime, client);

    let (border_cell, pane_id, border_side) = find_horizontal_border(&runtime, client);
    let initial_row_count = pane_row_count(&runtime, client, pane_id);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(border_cell.column, border_cell.row),
    );
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(
            border_cell.column,
            compute_outward_row(border_side, border_cell.row, 3),
        ),
    );

    assert_eq!(
        pane_row_count(&runtime, client, pane_id),
        initial_row_count + 3,
        "the grabbed pane grew by the three rows dragged toward its border"
    );
}

#[test]
fn grabbing_the_outer_frame_starts_no_resize() {
    let (mut runtime, client) = build_runtime();
    let mut viewer = build_viewer(&mut runtime, client);
    split_focused_pane(&mut runtime, client);

    let (border_cell, pane_id, border_side) = find_outer_vertical_frame(&runtime, client);
    assert_eq!(
        border_side,
        Direction::Right,
        "the rightmost mouse_frame is a right border"
    );
    let initial_column_count = pane_column_count(&runtime, client, pane_id);

    // The outer mouse_frame sits at the tab edge and cannot move, so grabbing it starts
    // no resize drag.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(border_cell.column, border_cell.row),
    );

    // A drag inward after that changes nothing either.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(border_cell.column - 3, border_cell.row),
    );
    assert_eq!(
        pane_column_count(&runtime, client, pane_id),
        initial_column_count,
        "grabbing the terminal's outer edge resizes nothing"
    );
}

#[test]
fn grabbing_the_frame_of_a_fullscreen_pane_starts_no_resize() {
    let (mut runtime, client) = build_runtime();
    let mut viewer = build_viewer(&mut runtime, client);
    split_focused_pane(&mut runtime, client);
    let (_, pane_id, _) = find_vertical_border(&runtime, client);
    let tiled_column_count = pane_column_count(&runtime, client, pane_id);

    // Zoom the focused pane: its border ring is now the outer mouse_frame, while the
    // tiled tree underneath still has a hidden neighbor to its side.
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client),
        SystemTime::now(),
        Command::TogglePaneFullscreen,
    );
    let _ = runtime.dispatch(command_envelope);
    let active_tab_id = runtime.get_client_mut(client).unwrap().get_active_tab();
    let fullscreen_pane_id = runtime.find_typed_pane(client).expect("a focused pane");

    // Grab the zoomed pane's right mouse_frame edge and drag inward: no divider is
    // visible under a zoom, so no resize begins, the zoom stands, and the
    // hidden tiled layout is untouched.
    let (border_cell, _, _) = find_vertical_border(&runtime, client);
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(border_cell.column, border_cell.row),
    );

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(border_cell.column - 3, border_cell.row),
    );
    assert_eq!(
        runtime
            .get_client_mut(client)
            .unwrap()
            .get_layout_mode(active_tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: fullscreen_pane_id,
        },
        "no resize was dispatched, so the client's zoom stands"
    );

    // Toggle back out: the tiled layout is exactly as it was.
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client),
        SystemTime::now(),
        Command::TogglePaneFullscreen,
    );
    let _ = runtime.dispatch(command_envelope);
    assert_eq!(
        pane_column_count(&runtime, client, pane_id),
        tiled_column_count,
        "the hidden tiled layout was not mutated by the drag"
    );
}

#[test]
fn a_click_in_the_focused_pane_forwards_a_report_when_the_program_asks() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let mut viewer = build_viewer(&mut runtime, client);
    let pane = get_only_pane_id(&runtime);
    // The program turns on normal tracking with SGR encoding.
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    let (screen_point, pane_column, pane_row) = find_pane_content_cell(&runtime, client, pane);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(screen_point.column, screen_point.row),
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        vec![format!("\x1b[<0;{pane_column};{pane_row}M").into_bytes()],
        "the click in the focused pane is forwarded as an SGR report"
    );
}

#[test]
fn a_click_forwards_nothing_when_the_program_wants_no_mouse() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let mut viewer = build_viewer(&mut runtime, client);
    let pane = get_only_pane_id(&runtime);
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(screen_point.column, screen_point.row),
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        Vec::<Vec<u8>>::new(),
        "a pane in no mouse mode receives nothing"
    );
}

#[test]
fn a_press_drag_release_gesture_forwards_each_event() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let mut viewer = build_viewer(&mut runtime, client);
    let pane = get_only_pane_id(&runtime);
    // Button-event tracking reports drags; SGR encoding.
    runtime.handle_pty_output(pane, b"\x1b[?1002h\x1b[?1006h");
    let (screen_point, pane_column, pane_row) = find_pane_content_cell(&runtime, client, pane);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(screen_point.column, screen_point.row),
    );
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(screen_point.column, screen_point.row),
    );
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        MouseInput {
            mouse_kind: MouseKind::Release(MouseButton::Left),
            position: screen_point,
            modifier_flags: ModFlags::NONE,
        },
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        vec![
            format!("\x1b[<0;{pane_column};{pane_row}M").into_bytes(),
            format!("\x1b[<32;{pane_column};{pane_row}M").into_bytes(),
            format!("\x1b[<0;{pane_column};{pane_row}m").into_bytes(),
        ],
        "press, then drag with the motion bit, then release with a lowercase m"
    );
}

#[test]
fn a_drag_reports_the_cell_it_moved_to_with_the_column_and_row_the_right_way_round() {
    // Every other forwarding test presses `find_pane_content_cell`, which is the pane's
    // top-left content cell — column 1, row 1. Two equal numbers cannot show
    // which is which, so those tests pass just as well with the pair swapped.
    // This one moves three columns across and one row down, where a swap reads
    // `4;2` as `2;4`.
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let mut viewer = build_viewer(&mut runtime, client);
    let pane = get_only_pane_id(&runtime);
    // Button-event tracking reports drags; SGR encoding.
    runtime.handle_pty_output(pane, b"\x1b[?1002h\x1b[?1006h");
    let (start_point, pane_column, pane_row) = find_pane_content_cell(&runtime, client, pane);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(start_point.column, start_point.row),
    );
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(start_point.column + 3, start_point.row + 1),
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        vec![
            format!("\x1b[<0;{pane_column};{pane_row}M").into_bytes(),
            format!("\x1b[<32;{};{}M", pane_column + 3, pane_row + 1).into_bytes(),
        ],
        "the drag reports the cell it moved to, column first"
    );
}

#[test]
fn a_mouse_select_gesture_over_a_mouse_aware_program_forwards_nothing() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let mut viewer = build_viewer(&mut runtime, client);
    let pane = get_only_pane_id(&runtime);
    // Button-event tracking would report a bare drag; SGR encoding.
    runtime.handle_pty_output(pane, b"\x1b[?1002h\x1b[?1006h");
    // Grab the mouse for koshi selection, the way the binding does; the viewer
    // takes the change off its subscription before the next event.
    let _ = runtime.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client),
        SystemTime::now(),
        Command::ToggleMouseSelect,
    ));
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);
    let mouse_gesture = |mouse_kind| MouseInput {
        mouse_kind,
        position: screen_point,
        modifier_flags: ModFlags::NONE,
    };

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        mouse_gesture(MouseKind::Press(MouseButton::Left)),
    );
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        mouse_gesture(MouseKind::Drag(MouseButton::Left)),
    );
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        mouse_gesture(MouseKind::Release(MouseButton::Left)),
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        Vec::<Vec<u8>>::new(),
        "in mouse-select mode the gesture is koshi's selection; the program is sent nothing"
    );
}

#[test]
fn the_forward_door_reports_whether_the_pane_was_written_to() {
    // The report the door gives back is what the viewer captures a gesture on,
    // so it must be false for exactly the events the pane never saw.
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    let (screen_point, pane_column, pane_row) = find_pane_content_cell(&runtime, client, pane);

    // The program asked for no mouse: nothing is written, and the door says so.
    assert!(!runtime.forward_mouse_to_pane(
        client,
        pane,
        build_left_press_input(screen_point.column, screen_point.row),
    ));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        Vec::<Vec<u8>>::new(),
        "a pane in no mouse mode receives nothing"
    );

    // Normal tracking with SGR encoding: the press is written, and reported.
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    assert!(runtime.forward_mouse_to_pane(
        client,
        pane,
        build_left_press_input(screen_point.column, screen_point.row),
    ));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        vec![format!("\x1b[<0;{pane_column};{pane_row}M").into_bytes()],
        "the press reached the program"
    );

    // The pane refuses the bytes: the door says so, and no gesture is captured
    // on the strength of a press that never landed.
    fake_pty_backend.fail_writes_on(pane, PtyError::UnknownPane { pane_id: pane });
    assert!(!runtime.forward_mouse_to_pane(
        client,
        pane,
        build_left_press_input(screen_point.column, screen_point.row),
    ));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        vec![format!("\x1b[<0;{pane_column};{pane_row}M").into_bytes()],
        "the refused press left no record"
    );
}

/// A tab whose only viewer reports [`PaneArea::Starving`] has no effective
/// size: every pane is suppressed and the click is forwarded to no pane.
#[test]
fn a_click_from_a_starving_sole_viewer_forwards_nothing() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    // Read the cell off the layout while the client still sizes the tab.
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");

    runtime
        .get_session_for_client_mut(client)
        .expect("session")
        .clients
        .get_client_mut_by_id(client)
        .expect("client")
        .update_pane_area(Some(PaneArea::Starving));

    assert!(!runtime.forward_mouse_to_pane(
        client,
        pane,
        build_left_press_input(screen_point.column, screen_point.row),
    ));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_bare_move_forwards_only_in_any_motion_mode() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let mut viewer = build_viewer(&mut runtime, client);
    let pane = get_only_pane_id(&runtime);
    let (screen_point, pane_column, pane_row) = find_pane_content_cell(&runtime, client, pane);
    let motion_input = MouseInput {
        mouse_kind: MouseKind::Motion,
        position: screen_point,
        modifier_flags: ModFlags::NONE,
    };

    // Normal tracking does not report motion: the move forwards nothing (and the
    // mouse_frame is never rebuilt to check).
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    dispatch_mouse_input(&mut runtime, &mut viewer, motion_input);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        Vec::<Vec<u8>>::new(),
        "normal tracking ignores a bare move"
    );

    // Any-motion tracking reports it: no-button 3 + motion bit 32 = 35.
    runtime.handle_pty_output(pane, b"\x1b[?1003h");
    dispatch_mouse_input(&mut runtime, &mut viewer, motion_input);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        vec![format!("\x1b[<35;{pane_column};{pane_row}M").into_bytes()],
        "any-motion tracking reports the move"
    );
}

#[test]
fn a_captured_release_is_re_stamped_to_the_pressed_button() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let mut viewer = build_viewer(&mut runtime, client);
    let pane = get_only_pane_id(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    let (screen_point, pane_column, pane_row) = find_pane_content_cell(&runtime, client, pane);

    // A right press captures the gesture (button 2).
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        MouseInput {
            mouse_kind: MouseKind::Press(MouseButton::Right),
            position: screen_point,
            modifier_flags: ModFlags::NONE,
        },
    );
    // The terminal reports the release as the left button (a stand-in); it must
    // still reach the program as a right release, matching the press.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        MouseInput {
            mouse_kind: MouseKind::Release(MouseButton::Left),
            position: screen_point,
            modifier_flags: ModFlags::NONE,
        },
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        vec![
            format!("\x1b[<2;{pane_column};{pane_row}M").into_bytes(),
            format!("\x1b[<2;{pane_column};{pane_row}m").into_bytes(),
        ],
        "the release re-stamps to button 2, not the reported left button 0"
    );
}

#[test]
fn a_drag_with_no_captured_press_is_dropped() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let mut viewer = build_viewer(&mut runtime, client);
    let pane = get_only_pane_id(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1002h\x1b[?1006h");
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);

    // A drag arrives without a press to capture the gesture (a release with no
    // matching press is the orphan-release case) — nothing is forwarded.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(screen_point.column, screen_point.row),
    );
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        MouseInput {
            mouse_kind: MouseKind::Release(MouseButton::Left),
            position: screen_point,
            modifier_flags: ModFlags::NONE,
        },
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        Vec::<Vec<u8>>::new(),
        "a gesture with no captured press forwards nothing"
    );
}

#[test]
fn a_captured_drag_that_leaves_the_pane_clamps_to_its_edge() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let mut viewer = build_viewer(&mut runtime, client);
    let pane = get_only_pane_id(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1002h\x1b[?1006h");
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);

    // Press inside the pane to capture the gesture, then drag far past its top-
    // left corner (0, 0 is the tabline row, outside the pane); the captured drag
    // clamps to the pane's first cell.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(screen_point.column, screen_point.row),
    );
    dispatch_mouse_input(&mut runtime, &mut viewer, build_left_drag_input(0, 0));

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes")
            .last()
            .expect("a drag"),
        &b"\x1b[<32;1;1M".to_vec(),
        "the drag clamps to the pane's top-left cell (1, 1)"
    );
}

#[test]
fn border_resize_off_leaves_a_border_press_inert() {
    let (mut runtime, client) = build_runtime();
    // The setting is the viewer's own, so it is the viewer that must be built
    // on it.
    let mut viewer = build_viewer(&mut runtime, client);
    viewer.load_startup_config(
        Some(PartialKoshiConfig {
            mouse: Some(PartialMouseConfig {
                can_resize_pane_border: Some(false),
                ..PartialMouseConfig::default()
            }),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );
    split_focused_pane(&mut runtime, client);

    let (border_cell, pane_id, border_side) = find_vertical_border(&runtime, client);
    let initial_column_count = pane_column_count(&runtime, client, pane_id);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(border_cell.column, border_cell.row),
    );
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_drag_input(
            compute_outward_column(border_side, border_cell.column, 3),
            border_cell.row,
        ),
    );

    assert_eq!(
        pane_column_count(&runtime, client, pane_id),
        initial_column_count,
        "with border resize disabled, a border drag changes nothing"
    );
}

#[test]
fn a_click_on_an_unfocused_pane_focuses_it_rather_than_forwarding() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let mut viewer = build_viewer(&mut runtime, client);
    split_focused_pane(&mut runtime, client);
    let focused_pane_id = runtime.find_typed_pane(client).expect("a focused pane");

    // The other pane in the split is not focused; both had mouse mode on, so a
    // forward would have written bytes.
    let render_snapshot = runtime.build_snapshot(client).expect("render snapshot");
    let other_pane_id = render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| pane_slot.pane_id)
        .find(|&pane_id| pane_id != focused_pane_id)
        .expect("a second pane");
    runtime.handle_pty_output(other_pane_id, b"\x1b[?1000h\x1b[?1006h");
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, other_pane_id);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_left_press_input(screen_point.column, screen_point.row),
    );

    assert_eq!(
        runtime.find_typed_pane(client),
        Some(other_pane_id),
        "the click moved focus"
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(other_pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new(),
        "the first click only focuses; it is not forwarded"
    );
}

/// A wheel event at a screen cell.
fn build_wheel_input(direction: ScrollDirection, screen_point: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Scroll(direction),
        position: screen_point,
        modifier_flags: ModFlags::NONE,
    }
}

/// The scrollback view offset for `pane_id` as seen by `client_id`.
fn get_pane_scroll_offset(runtime: &Server, client_id: ClientId, pane_id: PaneId) -> usize {
    runtime
        .list_sessions()
        .values()
        .next()
        .unwrap()
        .clients
        .get_client_by_id(client_id)
        .unwrap()
        .get_scroll_offset(pane_id)
}

/// Whether `client_id` has a highlight in `pane_id`.
fn has_pane_highlight(runtime: &Server, client_id: ClientId, pane_id: PaneId) -> bool {
    runtime
        .list_sessions()
        .values()
        .next()
        .unwrap()
        .clients
        .get_client_by_id(client_id)
        .unwrap()
        .get_selection(pane_id)
        .is_some()
}

/// The pane the viewer's pointer is marked as hovering over.
fn hovered_pane_id(runtime: &Server, viewer: &ViewerClient) -> Option<PaneId> {
    let render_snapshot = runtime
        .build_snapshot(viewer.get_client_id())
        .expect("render snapshot");
    viewer
        .build_viewer_chrome(render_snapshot.client_snapshot.active_tab_id)
        .hovered_pane_id
}

/// Fill `pane_id`'s scrollback with `line_count` lines by printing that many newlines,
/// so a scroll up has room to move.
fn feed_pane_scrollback(runtime: &mut Server, pane_id: PaneId, line_count: usize) {
    for _line_index in 0..line_count {
        runtime.handle_pty_output(pane_id, b"x\r\n");
    }
}

/// Put a highlight in `pane_id`, as a drag would, so the view is held.
fn set_pane_highlight(runtime: &mut Server, client_id: ClientId, pane_id: PaneId) {
    runtime.get_client_mut(client_id).unwrap().set_selection(
        pane_id,
        Selection {
            selection_kind: SelectionKind::Character,
            anchor: GridPosition {
                row_index: 0,
                column_index: 0,
            },
            cursor: GridPosition {
                row_index: 0,
                column_index: 4,
            },
        },
    );
}

#[test]
fn a_wheel_over_a_plain_pane_scrolls_its_scrollback() {
    let (mut runtime, client) = build_runtime();
    let pane = get_only_pane_id(&runtime);
    feed_pane_scrollback(&mut runtime, pane, 40);
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);
    let mut viewer = build_viewer(&mut runtime, client);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Up, screen_point),
    );

    // scroll_lines defaults to 3, so one wheel up moves the view three lines.
    assert_eq!(
        get_pane_scroll_offset(&runtime, client, pane),
        3,
        "wheel up scrolls"
    );
    assert_eq!(
        get_tabline_offset(&runtime, &viewer),
        None,
        "the pane wheel leaves the tab strip alone"
    );
}

#[test]
fn a_wheel_down_returns_the_view_toward_live() {
    let (mut runtime, client) = build_runtime();
    let pane = get_only_pane_id(&runtime);
    feed_pane_scrollback(&mut runtime, pane, 40);
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);

    let mut viewer = build_viewer(&mut runtime, client);
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Up, screen_point),
    );
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Up, screen_point),
    );
    assert_eq!(
        get_pane_scroll_offset(&runtime, client, pane),
        6,
        "two ups, six lines"
    );

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Down, screen_point),
    );
    assert_eq!(
        get_pane_scroll_offset(&runtime, client, pane),
        3,
        "a wheel down walks the view back three lines"
    );
}

#[test]
fn a_wheel_with_a_highlight_up_scrolls_and_keeps_the_highlight() {
    let (mut runtime, client) = build_runtime();
    let pane = get_only_pane_id(&runtime);
    feed_pane_scrollback(&mut runtime, pane, 40);
    set_pane_highlight(&mut runtime, client, pane);
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);
    let mut viewer = build_viewer(&mut runtime, client);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Up, screen_point),
    );

    assert_eq!(
        get_pane_scroll_offset(&runtime, client, pane),
        3,
        "a highlighted view still scrolls on the wheel"
    );
    assert!(
        has_pane_highlight(&runtime, client, pane),
        "the wheel holds the highlight; it does not clear it"
    );
}

#[test]
fn a_wheel_over_a_mouse_reporting_pane_forwards_a_report() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    feed_pane_scrollback(&mut runtime, pane, 40);
    // The program turns on normal tracking with SGR encoding.
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    let (screen_point, pane_column, pane_row) = find_pane_content_cell(&runtime, client, pane);
    let mut viewer = build_viewer(&mut runtime, client);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Up, screen_point),
    );

    // Wheel up is SGR button 64; the program gets it, and koshi does not scroll.
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        vec![format!("\x1b[<64;{pane_column};{pane_row}M").into_bytes()],
        "the wheel is forwarded as a mouse report"
    );
    assert_eq!(
        get_pane_scroll_offset(&runtime, client, pane),
        0,
        "a mouse-reporting pane keeps its own scrollback still"
    );
}

#[test]
fn a_wheel_on_the_alternate_screen_with_alt_scroll_sends_arrow_keys() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    // Enter the alternate screen and turn alternate-scroll on, with no mouse mode.
    runtime.handle_pty_output(pane, b"\x1b[?1049h\x1b[?1007h");
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);
    let mut viewer = build_viewer(&mut runtime, client);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Up, screen_point),
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        vec![b"\x1b[A\x1b[A\x1b[A".to_vec()],
        "wheel up becomes three up-arrows under default cursor keys"
    );

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Down, screen_point),
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes")
            .last()
            .expect("a write"),
        &b"\x1b[B\x1b[B\x1b[B".to_vec(),
        "wheel down becomes three down-arrows"
    );
}

#[test]
fn alt_scroll_uses_application_cursor_keys_when_the_program_asks() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    // Alternate screen, alternate-scroll on, application cursor keys on.
    runtime.handle_pty_output(pane, b"\x1b[?1049h\x1b[?1007h\x1b[?1h");
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);
    let mut viewer = build_viewer(&mut runtime, client);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Up, screen_point),
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        vec![b"\x1bOA\x1bOA\x1bOA".to_vec()],
        "application cursor keys send the SS3 form ESC O A"
    );
}

#[test]
fn the_ignore_wheel_config_does_nothing_over_a_plain_pane() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    feed_pane_scrollback(&mut runtime, pane, 40);
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);
    // The setting is the viewer's own, so it is the viewer that must be built
    // on it.
    let mut viewer = build_viewer_with_wheel(&mut runtime, client, WheelScroll::Ignore);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Up, screen_point),
    );

    assert_eq!(
        get_pane_scroll_offset(&runtime, client, pane),
        0,
        "the ignore setting leaves the view where it is"
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        Vec::<Vec<u8>>::new(),
        "the ignore setting forwards nothing either"
    );
}

#[test]
fn a_horizontal_wheel_does_not_scroll_the_scrollback() {
    let (mut runtime, client) = build_runtime();
    let pane = get_only_pane_id(&runtime);
    feed_pane_scrollback(&mut runtime, pane, 40);
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);

    let mut viewer = build_viewer(&mut runtime, client);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Left, screen_point),
    );

    assert_eq!(
        get_pane_scroll_offset(&runtime, client, pane),
        0,
        "a horizontal wheel leaves the vertical scrollback view alone"
    );
}

#[test]
fn a_move_marks_the_hovered_pane_and_clears_it_off_a_pane() {
    let (mut runtime, client) = build_runtime();
    let mut viewer = build_viewer(&mut runtime, client);
    let pane = get_only_pane_id(&runtime);
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        MouseInput {
            mouse_kind: MouseKind::Motion,
            position: screen_point,
            modifier_flags: ModFlags::NONE,
        },
    );
    assert_eq!(
        hovered_pane_id(&runtime, &viewer),
        Some(pane),
        "a move over pane content marks it hovered"
    );

    // Row 0 is the tabline, not a pane.
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        MouseInput {
            mouse_kind: MouseKind::Motion,
            position: Point { column: 0, row: 0 },
            modifier_flags: ModFlags::NONE,
        },
    );
    assert_eq!(
        hovered_pane_id(&runtime, &viewer),
        None,
        "a move onto chrome clears the hover"
    );
}

#[test]
fn a_wheel_scrolls_the_pane_under_the_pointer_not_the_focused_one() {
    let (mut runtime, client) = build_runtime();
    split_focused_pane(&mut runtime, client);
    let focused_pane_id = runtime.find_typed_pane(client).expect("a focused pane");
    let render_snapshot = runtime.build_snapshot(client).expect("render snapshot");
    let other_pane_id = render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| pane_slot.pane_id)
        .find(|&pane_id| pane_id != focused_pane_id)
        .expect("a second pane");

    feed_pane_scrollback(&mut runtime, other_pane_id, 40);
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, other_pane_id);
    let mut viewer = build_viewer(&mut runtime, client);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Up, screen_point),
    );

    assert_eq!(
        get_pane_scroll_offset(&runtime, client, other_pane_id),
        3,
        "the pane under the pointer scrolls"
    );
    assert_eq!(
        get_pane_scroll_offset(&runtime, client, focused_pane_id),
        0,
        "the focused pane is left alone"
    );
}

#[test]
fn a_wheel_over_a_pane_border_scrolls_the_focused_pane() {
    let (mut runtime, client) = build_runtime();
    split_focused_pane(&mut runtime, client);
    let focused_pane_id = runtime.find_typed_pane(client).expect("a focused pane");
    feed_pane_scrollback(&mut runtime, focused_pane_id, 40);

    // The divider between the two panes is chrome, not pane content: a wheel
    // there has no pane under the pointer, so it falls to the focused pane.
    let (border_cell, _, _) = find_vertical_border(&runtime, client);
    let mut viewer = build_viewer(&mut runtime, client);
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Up, border_cell),
    );

    assert_eq!(
        get_pane_scroll_offset(&runtime, client, focused_pane_id),
        3,
        "a wheel over chrome scrolls the focused pane"
    );
}

#[test]
fn a_wheel_over_an_unfocused_mouse_app_forwards_to_that_pane() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    split_focused_pane(&mut runtime, client);
    let focused_pane_id = runtime.find_typed_pane(client).expect("a focused pane");
    let render_snapshot = runtime.build_snapshot(client).expect("render snapshot");
    let other_pane_id = render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| pane_slot.pane_id)
        .find(|&pane_id| pane_id != focused_pane_id)
        .expect("a second pane");

    // The unfocused pane's program wants the mouse: normal tracking, SGR.
    runtime.handle_pty_output(other_pane_id, b"\x1b[?1000h\x1b[?1006h");
    let (screen_point, pane_column, pane_row) =
        find_pane_content_cell(&runtime, client, other_pane_id);
    let mut viewer = build_viewer(&mut runtime, client);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Up, screen_point),
    );

    // Wheel up is SGR button 64; it reaches the pane under the pointer even
    // though that pane is unfocused, and the focused pane gets nothing.
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(other_pane_id)
            .expect("writes"),
        vec![format!("\x1b[<64;{pane_column};{pane_row}M").into_bytes()],
        "the wheel forwards to the unfocused pane under the pointer"
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(focused_pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new(),
        "the focused pane receives nothing"
    );
}

#[test]
fn a_highlight_holds_the_view_even_over_a_mouse_reporting_program() {
    // The mouse_frame carries whether this client has a highlight in the pane, and a
    // highlight outranks the program's mouse mode: koshi scrolls its own view
    // and the program is told nothing.
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    feed_pane_scrollback(&mut runtime, pane, 40);
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    set_pane_highlight(&mut runtime, client, pane);
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);
    let mut viewer = build_viewer(&mut runtime, client);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Up, screen_point),
    );

    assert_eq!(
        get_pane_scroll_offset(&runtime, client, pane),
        3,
        "the highlighted view scrolls"
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        Vec::<Vec<u8>>::new(),
        "the program receives no report"
    );
}

#[test]
fn a_forwarded_wheel_is_dropped_when_the_program_turned_the_mouse_off() {
    // The viewer decides from the mouse_frame it painted, so it can name a pane whose
    // program has since stopped asking for the mouse. The session re-reads the
    // live mode when it writes and drops the wheel_input.
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);
    let mut viewer = build_viewer(&mut runtime, client);
    let wheel_input = build_wheel_input(ScrollDirection::Up, screen_point);
    let mouse_frame = MouseFrame::from(runtime.build_snapshot(client).expect("render snapshot"));
    let mouse_decision = viewer
        .handle_mouse_wheel(wheel_input, &mouse_frame)
        .expect("a wheel event decides");
    assert_eq!(
        mouse_decision.mouse_action,
        Some(MouseAction::Forward {
            pane_id: pane,
            mouse_input: wheel_input,
        }),
        "the painted mouse_frame still said the program wanted the mouse"
    );

    // The program turns mouse reporting off between that mouse_frame and the write.
    runtime.handle_pty_output(pane, b"\x1b[?1000l");
    runtime.forward_mouse_to_pane(client, pane, wheel_input);

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        Vec::<Vec<u8>>::new(),
        "nothing is written to a program that no longer wants the mouse"
    );
}

#[test]
fn alt_scroll_arrows_follow_the_cursor_key_mode_at_the_moment_they_are_written() {
    // DECCKM (`?1`) is read when the arrows are written, not when the mouse_frame the
    // viewer decided from was painted.
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1049h\x1b[?1007h");
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);
    let mut viewer = build_viewer(&mut runtime, client);
    let mouse_frame = MouseFrame::from(runtime.build_snapshot(client).expect("render snapshot"));
    let mouse_decision = viewer
        .handle_mouse_wheel(
            build_wheel_input(ScrollDirection::Up, screen_point),
            &mouse_frame,
        )
        .expect("a wheel event decides");
    assert_eq!(
        mouse_decision.mouse_action,
        Some(MouseAction::AltScrollArrows {
            pane_id: pane,
            is_scrolling_up: true,
            arrow_count: 3,
        })
    );

    // The program switches to application cursor keys before the write.
    runtime.handle_pty_output(pane, b"\x1b[?1h");
    runtime.write_alt_scroll_arrows(pane, true, 3);

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        vec![b"\x1bOA\x1bOA\x1bOA".to_vec()],
        "the SS3 form the live mode asks for, not the mouse_frame's"
    );
}

#[test]
fn arrow_keys_are_dropped_when_the_pane_left_the_alternate_screen_before_the_write() {
    // The viewer decides from the mouse_frame it painted, so it can name a pane whose
    // program has since left the alternate screen. Writing the arrows then would
    // put them in the shell prompt underneath and recall its history.
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1049h\x1b[?1007h");
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);
    let mut viewer = build_viewer(&mut runtime, client);
    let mouse_frame = MouseFrame::from(runtime.build_snapshot(client).expect("render snapshot"));
    let mouse_decision = viewer
        .handle_mouse_wheel(
            build_wheel_input(ScrollDirection::Up, screen_point),
            &mouse_frame,
        )
        .expect("a wheel event decides");
    assert_eq!(
        mouse_decision.mouse_action,
        Some(MouseAction::AltScrollArrows {
            pane_id: pane,
            is_scrolling_up: true,
            arrow_count: 3,
        }),
        "the painted mouse_frame still said the pane was on the alternate screen"
    );

    // The program leaves the alternate screen before the write.
    runtime.handle_pty_output(pane, b"\x1b[?1049l");
    runtime.write_alt_scroll_arrows(pane, true, 3);

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        Vec::<Vec<u8>>::new(),
        "nothing is written to a pane that is back on the primary screen"
    );
}

#[test]
fn the_write_doors_do_nothing_for_a_pane_that_is_gone() {
    // The viewer names a pane off a mouse_frame it painted, so it can name one the
    // session has since released. Every door must answer that with nothing.
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let live_pane_id = get_only_pane_id(&runtime);
    let missing_pane_id = PaneId::new();

    let view_top_row_index = runtime.scroll_pane_view(client, missing_pane_id, true, 3);
    let was_forwarded = runtime.forward_mouse_to_pane(
        client,
        missing_pane_id,
        build_wheel_input(ScrollDirection::Up, Point { column: 5, row: 5 }),
    );
    runtime.write_alt_scroll_arrows(missing_pane_id, true, 3);
    let applied_cell_count = runtime.drag_resize(client, missing_pane_id, Direction::Right, 1, 3);

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(missing_pane_id)
            .expect_err("no write log"),
        PtyError::UnknownPane {
            pane_id: missing_pane_id,
        },
        "a missing pane was never opened, so it has no write log"
    );
    assert_eq!(
        view_top_row_index, None,
        "a missing pane reports no top row"
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(live_pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new(),
        "nothing landed on the live pane instead"
    );
    assert_eq!(
        get_pane_scroll_offset(&runtime, client, missing_pane_id),
        0,
        "no view was stored for a missing pane"
    );
    assert_eq!(applied_cell_count, 0, "no border of a missing pane moved");
    assert!(!was_forwarded, "no report was written to a missing pane");
}

#[test]
fn a_zero_line_notch_sends_no_arrow_keys() {
    // `mouse.scroll_lines 0` reaches the door as a count of zero.
    let (mut runtime, fake_pty_backend, _client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1049h\x1b[?1007h");

    runtime.write_alt_scroll_arrows(pane, true, 0);

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        Vec::<Vec<u8>>::new(),
        "a zero-line notch sends no arrows at all"
    );
}

#[test]
fn a_one_line_notch_sends_one_arrow() {
    let (mut runtime, fake_pty_backend, _client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1049h\x1b[?1007h");

    runtime.write_alt_scroll_arrows(pane, false, 1);

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        vec![b"\x1b[B".to_vec()],
        "a one-line notch sends exactly one down-arrow"
    );
}

#[test]
fn a_scroll_of_a_pane_on_the_alternate_screen_stores_no_offset() {
    let (mut runtime, _fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    feed_pane_scrollback(&mut runtime, pane, 40);
    // Enter the alternate screen; the door is called straight, with no viewer
    // deciding anything first.
    runtime.handle_pty_output(pane, b"\x1b[?1049h");

    let view_top_row_index = runtime.scroll_pane_view(client, pane, true, 5);

    assert_eq!(
        get_pane_scroll_offset(&runtime, client, pane),
        0,
        "a pane on the alternate screen stores no offset"
    );
    // 40 lines through a 20-row pane push 21 into history, so the primary
    // screen's live view starts at line 21.
    assert_eq!(
        view_top_row_index,
        Some(21),
        "the answer is the live top row"
    );
}

#[test]
fn a_scroll_for_a_client_the_session_does_not_hold_moves_nothing() {
    let (mut runtime, _fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    feed_pane_scrollback(&mut runtime, pane, 40);
    let unattached_client_id = ClientId::new();

    let view_top_row_index = runtime.scroll_pane_view(unattached_client_id, pane, true, 5);

    // A client with no record has no stored offset, so it reads the live top
    // row: 40 lines through a 20-row pane push 21 into history.
    assert_eq!(
        view_top_row_index,
        Some(21),
        "the unattached client is answered the live top row"
    );
    assert_eq!(
        get_pane_scroll_offset(&runtime, client, pane),
        0,
        "and the session's own client's view did not move"
    );
}

#[test]
fn a_wheel_on_the_alternate_screen_without_alt_scroll_stores_no_offset() {
    let (mut runtime, client) = build_runtime();
    let pane = get_only_pane_id(&runtime);
    feed_pane_scrollback(&mut runtime, pane, 40);
    // Enter the alternate screen with neither mouse mode nor alt-scroll (?1007):
    // a full-screen app that ignores the wheel.
    runtime.handle_pty_output(pane, b"\x1b[?1049h");
    let (screen_point, _, _) = find_pane_content_cell(&runtime, client, pane);
    let mut viewer = build_viewer(&mut runtime, client);

    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Up, screen_point),
    );

    // The alternate screen keeps no scrollback, so the wheel stores no offset —
    // otherwise the shell would be scrolled back when the app exits.
    assert_eq!(
        get_pane_scroll_offset(&runtime, client, pane),
        0,
        "a wheel on the alternate screen leaves the primary offset at 0"
    );
}

/// A screen cell that is chrome, not any pane's content — a pane border, the
/// status line, or a gap — where a wheel falls through to the focused pane.
fn find_chrome_cell(runtime: &Server, client_id: ClientId) -> Point {
    let render_snapshot = runtime.build_snapshot(client_id).expect("render snapshot");
    let viewport_size = render_snapshot.client_snapshot.viewport_size;
    for row_index in 0..viewport_size.row_count {
        for column_index in 0..viewport_size.column_count {
            let screen_point = Point {
                column: column_index,
                row: row_index,
            };
            if matches!(
                hit_test(
                    render_snapshot.build_frame_layout(ViewerChrome::default()),
                    screen_point
                ),
                HitRegion::PaneBorder { .. } | HitRegion::Statusline | HitRegion::None
            ) {
                return screen_point;
            }
        }
    }
    panic!("no chrome cell in the render frame");
}

#[test]
fn a_wheel_over_chrome_reaches_the_focused_mouse_app() {
    let (mut runtime, fake_pty_backend, client) = build_runtime_with_fake_pty_backend();
    let pane = get_only_pane_id(&runtime);
    // The focused pane's program wants the mouse: normal tracking, SGR.
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");

    // A wheel over chrome (no pane under the pointer) goes to the focused pane,
    // clamped to its edge, instead of being dropped.
    let chrome_point = find_chrome_cell(&runtime, client);
    let mut viewer = build_viewer(&mut runtime, client);
    dispatch_mouse_input(
        &mut runtime,
        &mut viewer,
        build_wheel_input(ScrollDirection::Up, chrome_point),
    );

    assert_eq!(
        chrome_point,
        Point { column: 0, row: 1 },
        "the first chrome cell is the pane's own left border"
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane)
            .expect("writes"),
        vec![b"\x1b[<64;1;1M".to_vec()],
        "an SGR wheel-up report (button 64), clamped to the pane's first cell"
    );
}

#[test]
fn a_forward_decided_from_a_stale_frame_writes_nothing_once_tracking_is_off() {
    // The viewer answers from the mouse_frame it last painted, which can be one event
    // out of date. Here the program turns mouse reporting off after that mouse_frame
    // was painted and before the press is applied. The session reads the live
    // level at the moment of the write, so the program that stopped asking gets
    // nothing.
    let (mut server, fake_pty_backend, client_id) = build_runtime_with_fake_pty_backend();
    let mut viewer = build_viewer(&mut server, client_id);
    let pane_id = get_only_pane_id(&server);
    server.handle_pty_output(pane_id, b"\x1b[?1000h\x1b[?1006h");
    let (screen_point, _, _) = find_pane_content_cell(&server, client_id, pane_id);

    let mouse_frame = MouseFrame::from(server.build_snapshot(client_id).expect("render snapshot"));
    let press_input = build_left_press_input(screen_point.column, screen_point.row);
    let mouse_actions =
        viewer.handle_mouse(press_input, &mouse_frame, compute_separated_event_time());
    assert_eq!(
        mouse_actions,
        vec![MouseAction::Forward {
            pane_id,
            mouse_input: press_input,
        }],
        "the mouse_frame said the program wanted the mouse"
    );

    // The program turns reporting off; no mouse_frame is painted in between.
    server.handle_pty_output(pane_id, b"\x1b[?1000l");
    apply_mouse_actions(&mut server, &mut viewer, &mouse_frame, mouse_actions);

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new(),
        "the live level, not the painted one, decides what is written"
    );
}

/// Stack a second pane onto the focused one, so the tab holds one stack whose
/// members share a rect: the active member shows its content and the other
/// collapses to a one-row header strip.
fn stack_pane_onto_focused(server: &mut Server, client_id: ClientId) {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        SystemTime::now(),
        Command::NewPane(NewPaneArgs {
            should_stack: true,
            ..build_new_pane_args()
        }),
    );
    let _ = server.dispatch(command_envelope);
}

/// A border cell of a drawn pane that sits right against a collapsed stack
/// member's header strip. Panics if the mouse_frame has no such cell.
fn find_border_against_stack_header(server: &Server, client_id: ClientId) -> Point {
    let render_snapshot = server.build_snapshot(client_id).expect("render snapshot");
    let viewport_size = render_snapshot.client_snapshot.viewport_size;
    let hit_region_at = |screen_point: Point| {
        hit_test(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            screen_point,
        )
    };
    let column_index = viewport_size.column_count / 2;
    for row_index in 1..viewport_size.row_count - 1 {
        let HitRegion::PaneBorder {
            side: border_side, ..
        } = hit_region_at(Point {
            column: column_index,
            row: row_index,
        })
        else {
            continue;
        };
        let adjacent_region = match border_side {
            Direction::Up => hit_region_at(Point {
                column: column_index,
                row: row_index - 1,
            }),
            Direction::Down => hit_region_at(Point {
                column: column_index,
                row: row_index + 1,
            }),
            _ => continue,
        };
        if matches!(adjacent_region, HitRegion::StackHeader { .. }) {
            return Point {
                column: column_index,
                row: row_index,
            };
        }
    }
    panic!("no pane border against a stack header");
}

#[test]
fn grabbing_the_border_against_a_collapsed_stack_member_starts_no_resize() {
    // A collapsed stack member is drawn as a one-row header strip with no
    // content area, so there is no pane box on the far side of that boundary to
    // resize against. Grabbing it must begin no drag — and a stack shares one
    // rect anyway, so there is nothing a border move could redistribute.
    let (mut server, client_id) = build_runtime();
    let mut viewer = build_viewer(&mut server, client_id);
    stack_pane_onto_focused(&mut server, client_id);

    let border_cell = find_border_against_stack_header(&server, client_id);
    let mouse_frame = MouseFrame::from(server.build_snapshot(client_id).expect("render snapshot"));

    let pressed = viewer.handle_mouse(
        build_left_press_input(border_cell.column, border_cell.row),
        &mouse_frame,
        compute_separated_event_time(),
    );
    assert_eq!(
        pressed,
        Vec::new(),
        "the header strip is no neighbor to resize against, so the press \
         begins no drag"
    );

    let dragged = viewer.handle_mouse(
        build_left_drag_input(border_cell.column, border_cell.row + 3),
        &mouse_frame,
        compute_separated_event_time(),
    );
    assert_eq!(
        dragged,
        Vec::new(),
        "with no drag under way the pointer asks for no border move"
    );
}

/// The runtime, its fake PTY backend, a client, and that client's own
/// subscriber queue — the queue a mouse round's answer lands on.
fn build_server_with_mouse_event_queue() -> (
    Server,
    Arc<FakePtyBackend>,
    ClientId,
    mpsc::Receiver<Delivery>,
) {
    let (mut server, fake_pty_backend, client_id) = build_runtime_with_fake_pty_backend();
    let mouse_event_queue = server.subscribe(client_id, EventFilter::All);
    (server, fake_pty_backend, client_id, mouse_event_queue)
}

/// Every mouse answer waiting on `mouse_event_queue`, in order, as its round id
/// and its list. Render frames and other events are left out.
fn list_mouse_answers(
    mouse_event_queue: &mpsc::Receiver<Delivery>,
) -> Vec<(u64, Vec<MouseAnswer>)> {
    mouse_event_queue
        .try_iter()
        .filter_map(|delivery| match delivery {
            Delivery::MouseAnswer {
                request_id,
                mouse_answers,
            } => Some((request_id, mouse_answers)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_round_holding_one_scroll_answers_with_the_line_the_view_landed_on() {
    let (mut server, _fake_pty_backend, client_id, mouse_event_queue) =
        build_server_with_mouse_event_queue();
    let pane_id = get_only_pane_id(&server);
    feed_pane_scrollback(&mut server, pane_id, 40);

    server.run_client_mouse(
        client_id,
        7,
        vec![WireMouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 5,
        }],
    );

    assert_eq!(
        get_pane_scroll_offset(&server, client_id, pane_id),
        5,
        "the view moved five lines into history"
    );
    // A zero-line move reads the same line back off the door without touching
    // the view, so the answer is checked against the door's own number.
    let view_top_row_index = server.scroll_pane_view(client_id, pane_id, true, 0);
    // 40 lines through a 20-row pane push 21 into history, so the live view's
    // top row is line 21 and five lines up is line 16.
    assert_eq!(view_top_row_index, Some(16));
    assert_eq!(
        list_mouse_answers(&mouse_event_queue),
        vec![(
            7,
            vec![MouseAnswer::Scrolled {
                pane_id,
                top_row_number: view_top_row_index,
            }],
        )],
        "the round is answered with the line the scroll landed on"
    );
}

#[test]
fn a_round_holding_one_border_move_answers_with_the_cells_it_took() {
    let (mut server, _fake_pty_backend, client_id, mouse_event_queue) =
        build_server_with_mouse_event_queue();
    let pane_id = get_only_pane_id(&server);
    split_focused_pane(&mut server, client_id);
    let initial_column_count = pane_column_count(&server, client_id, pane_id);

    server.run_client_mouse(
        client_id,
        8,
        vec![WireMouseAction::Resize {
            pane_id,
            border_side: Direction::Right,
            resize_step: 1,
            requested_cell_count: 3,
        }],
    );

    assert_eq!(
        pane_column_count(&server, client_id, pane_id),
        initial_column_count + 3,
        "the border moved three cells"
    );
    assert_eq!(
        list_mouse_answers(&mouse_event_queue),
        vec![(
            8,
            vec![MouseAnswer::Resized {
                pane_id,
                border_side: Direction::Right,
                resize_step: 1,
                applied_cell_count: 3,
            }]
        )],
        "the answer names the border it moved and the cells it really took"
    );
}

#[test]
fn a_round_holding_one_forward_writes_the_report_and_answers_with_an_empty_list() {
    let (mut server, fake_pty_backend, client_id, mouse_event_queue) =
        build_server_with_mouse_event_queue();
    let pane_id = get_only_pane_id(&server);
    // The program turns on normal tracking with SGR encoding.
    server.handle_pty_output(pane_id, b"\x1b[?1000h\x1b[?1006h");
    let (screen_point, pane_column, pane_row) = find_pane_content_cell(&server, client_id, pane_id);

    server.run_client_mouse(
        client_id,
        9,
        vec![WireMouseAction::Forward {
            pane_id,
            mouse_input: build_left_press_input(screen_point.column, screen_point.row),
        }],
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![format!("\x1b[<0;{pane_column};{pane_row}M").into_bytes()],
        "the press reached the program as an SGR report"
    );
    assert_eq!(
        list_mouse_answers(&mouse_event_queue),
        vec![(9, Vec::new())],
        "a forward reports nothing, and the round is still answered"
    );
}

#[test]
fn a_round_holding_one_alt_scroll_writes_the_arrows_and_answers_with_an_empty_list() {
    let (mut server, fake_pty_backend, client_id, mouse_event_queue) =
        build_server_with_mouse_event_queue();
    let pane_id = get_only_pane_id(&server);
    // Enter the alternate screen and turn alternate-scroll on.
    server.handle_pty_output(pane_id, b"\x1b[?1049h\x1b[?1007h");

    server.run_client_mouse(
        client_id,
        10,
        vec![WireMouseAction::AltScrollArrows {
            pane_id,
            is_scrolling_up: true,
            arrow_count: 3,
        }],
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![b"\x1b[A\x1b[A\x1b[A".to_vec()],
        "wheel up became three up-arrows under default cursor keys"
    );
    assert_eq!(
        list_mouse_answers(&mouse_event_queue),
        vec![(10, Vec::new())],
        "arrows report nothing, and the round is still answered"
    );
}

#[test]
fn a_round_holding_one_command_runs_it_and_answers_with_an_empty_list() {
    let (mut server, _fake_pty_backend, client_id, mouse_event_queue) =
        build_server_with_mouse_event_queue();
    let count_tabs = |server: &Server| {
        server
            .build_snapshot(client_id)
            .expect("render snapshot")
            .session_snapshot
            .tabs_metadata
            .len()
    };
    assert_eq!(count_tabs(&server), 1, "the session starts on one tab");

    server.run_client_mouse(
        client_id,
        11,
        vec![WireMouseAction::Command(Box::new(Command::NewTab(
            NewTabArgs::default(),
        )))],
    );

    assert_eq!(count_tabs(&server), 2, "the command the round carried ran");
    assert_eq!(
        list_mouse_answers(&mouse_event_queue),
        vec![(11, Vec::new())],
        "a command reports nothing, and the round is still answered"
    );
}

#[test]
fn a_round_that_reports_nothing_is_still_answered() {
    // The answer is what releases the viewer's gate: skipping it stalls that
    // client's whole mouse uplink until it detaches.
    let (mut server, fake_pty_backend, client_id, mouse_event_queue) =
        build_server_with_mouse_event_queue();
    let pane_id = get_only_pane_id(&server);
    server.handle_pty_output(pane_id, b"\x1b[?1000h\x1b[?1006h");
    let (screen_point, pane_column, pane_row) = find_pane_content_cell(&server, client_id, pane_id);
    let forward_mouse_action = WireMouseAction::Forward {
        pane_id,
        mouse_input: build_left_press_input(screen_point.column, screen_point.row),
    };

    server.run_client_mouse(
        client_id,
        12,
        vec![forward_mouse_action.clone(), forward_mouse_action],
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![
            format!("\x1b[<0;{pane_column};{pane_row}M").into_bytes(),
            format!("\x1b[<0;{pane_column};{pane_row}M").into_bytes(),
        ],
        "both presses reached the program"
    );
    assert_eq!(
        list_mouse_answers(&mouse_event_queue),
        vec![(12, Vec::new())],
        "exactly one answer, holding an empty list"
    );
}

#[test]
fn a_round_with_no_actions_is_still_answered() {
    let (mut server, _fake_pty_backend, client_id, mouse_event_queue) =
        build_server_with_mouse_event_queue();

    server.run_client_mouse(client_id, 15, Vec::new());

    assert_eq!(
        list_mouse_answers(&mouse_event_queue),
        vec![(15, Vec::new())],
        "an empty round is answered with an empty list"
    );
}

#[test]
fn the_answers_follow_the_order_of_the_actions_that_reported() {
    let (mut server, fake_pty_backend, client_id, mouse_event_queue) =
        build_server_with_mouse_event_queue();
    let pane_id = get_only_pane_id(&server);
    split_focused_pane(&mut server, client_id);
    feed_pane_scrollback(&mut server, pane_id, 40);
    server.handle_pty_output(pane_id, b"\x1b[?1000h\x1b[?1006h");
    let (screen_point, pane_column, pane_row) = find_pane_content_cell(&server, client_id, pane_id);
    let initial_column_count = pane_column_count(&server, client_id, pane_id);

    server.run_client_mouse(
        client_id,
        13,
        vec![
            WireMouseAction::Scroll {
                pane_id,
                is_scrolling_up: true,
                scroll_line_count: 5,
            },
            WireMouseAction::Forward {
                pane_id,
                mouse_input: build_left_press_input(screen_point.column, screen_point.row),
            },
            WireMouseAction::Resize {
                pane_id,
                border_side: Direction::Right,
                resize_step: 1,
                requested_cell_count: 3,
            },
        ],
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![format!("\x1b[<0;{pane_column};{pane_row}M").into_bytes()],
        "the forward in the middle of the round still reached the program"
    );
    assert_eq!(
        pane_column_count(&server, client_id, pane_id),
        initial_column_count + 3,
        "the border move at the end of the round still ran"
    );
    assert_eq!(
        list_mouse_answers(&mouse_event_queue),
        vec![(
            13,
            vec![
                MouseAnswer::Scrolled {
                    pane_id,
                    top_row_number: Some(16),
                },
                MouseAnswer::Resized {
                    pane_id,
                    border_side: Direction::Right,
                    resize_step: 1,
                    applied_cell_count: 3,
                },
            ]
        )],
        "the scroll and the border move answer in that order; the forward is silent"
    );
}

#[test]
fn a_client_with_no_subscription_is_answered_nothing() {
    // A client with no subscription is no attached viewer: it waits on no
    // answer, so there is no gate to release.
    let (mut server, _fake_pty_backend, client_id) = build_runtime_with_fake_pty_backend();
    let pane_id = get_only_pane_id(&server);
    feed_pane_scrollback(&mut server, pane_id, 40);
    // Another client's queue, which would catch an answer sent to the wrong one.
    let unrelated_event_queue = server.subscribe(ClientId::new(), EventFilter::All);

    server.run_client_mouse(
        client_id,
        14,
        vec![WireMouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 5,
        }],
    );

    assert_eq!(
        get_pane_scroll_offset(&server, client_id, pane_id),
        5,
        "the round still ran"
    );
    assert_eq!(
        list_mouse_answers(&unrelated_event_queue),
        Vec::new(),
        "and nothing was answered"
    );
}

#[test]
fn a_two_cell_drag_moves_the_border_as_far_as_two_one_cell_drags_do() {
    // Five stacked panes on a 40-row viewport, on the stock 1-row pane
    // minimum. The donating pane's solved height does not change on the first
    // of the two cells, so the layout still has a cell to give after that
    // first one and only the second cell moves the border. Asking once and
    // retrying once for the spare stops a cell short here; asking again for
    // each fresh spare does not.
    let (mut server, _fake_pty_backend, client_id) = build_sized_runtime(Size {
        column_count: 80,
        row_count: 40,
    });
    for _ in 0..4 {
        split_focused_downward(&mut server, client_id);
    }
    let initial_pane_sizes = list_stacked_pane_sizes(&server, client_id);
    assert_eq!(
        list_pane_row_counts(&initial_pane_sizes),
        vec![19, 9, 4, 3, 3],
        "the five panes start at the heights this case needs"
    );

    let applied_cell_count =
        server.drag_resize(client_id, initial_pane_sizes[3].0, Direction::Up, 1, 2);

    assert_eq!(applied_cell_count, 2, "both cells of the drag were taken");
    assert_eq!(
        list_pane_row_counts(&list_stacked_pane_sizes(&server, client_id)),
        vec![19, 9, 3, 3, 4],
        "the border landed where two one-cell drags put it"
    );
}

#[test]
fn a_border_drag_of_zero_cells_moves_nothing_and_reports_zero() {
    let (mut server, _fake_pty_backend, client_id) = build_runtime_with_fake_pty_backend();
    let pane_id = get_only_pane_id(&server);
    split_focused_pane(&mut server, client_id);
    let initial_column_count = pane_column_count(&server, client_id, pane_id);

    let applied_cell_count = server.drag_resize(client_id, pane_id, Direction::Right, 1, 0);

    assert_eq!(applied_cell_count, 0, "a drag of no cells takes none");
    assert_eq!(
        pane_column_count(&server, client_id, pane_id),
        initial_column_count,
        "and the border did not move"
    );
}

#[test]
fn a_negative_step_moves_the_border_the_other_way() {
    let (mut server, _fake_pty_backend, client_id) = build_runtime_with_fake_pty_backend();
    let pane_id = get_only_pane_id(&server);
    split_focused_pane(&mut server, client_id);
    let initial_column_count = pane_column_count(&server, client_id, pane_id);

    let applied_cell_count = server.drag_resize(client_id, pane_id, Direction::Right, -1, 4);

    assert_eq!(
        applied_cell_count, 4,
        "all four cells of the shrink were taken"
    );
    assert_eq!(
        pane_column_count(&server, client_id, pane_id),
        initial_column_count - 4,
        "a step of -1 shrinks the grabbed pane"
    );
}

#[test]
fn a_border_drag_into_a_neighbor_at_its_minimum_reports_no_cells_taken() {
    let (mut server, _fake_pty_backend, client_id) = build_runtime_with_fake_pty_backend();
    let pane_id = get_only_pane_id(&server);
    split_focused_pane(&mut server, client_id);

    // Ask for far more than the neighbor can donate: the drag fills right up to
    // the neighbor's minimum size and reports the cells it really took.
    let filled_cell_count = server.drag_resize(client_id, pane_id, Direction::Right, 1, 200);
    assert_eq!(
        filled_cell_count, 36,
        "the neighbor gave every column above its minimum"
    );
    assert_eq!(
        pane_column_count(&server, client_id, pane_id),
        76,
        "40 columns plus 36"
    );

    let applied_cell_count = server.drag_resize(client_id, pane_id, Direction::Right, 1, 5);

    assert_eq!(
        applied_cell_count, 0,
        "a neighbor at its minimum donates nothing"
    );
    assert_eq!(
        pane_column_count(&server, client_id, pane_id),
        76,
        "and the border stayed at the wall"
    );
}
