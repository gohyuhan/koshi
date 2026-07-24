//! Mouse routing tests: a tab click focuses the tab and clears the peek, a
//! scroll arrow and the wheel peek the strip, and a tabline drag scrolls it.
//!
//! Client state is read back through [`Server::build_snapshot`] — the same
//! projection the renderer draws — so a test never reaches into private client
//! fields.

use super::*;

use std::sync::{mpsc, Arc};
use std::time::Duration;

use koshi_core::command::{GridPos, NewPaneArgs, NewTabArgs, Selection, SelectionKind};
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::SessionId;
use koshi_core::key::ModFlags;
use koshi_renderer::hit_test;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage};

fn runtime() -> (Server, ClientId) {
    let (runtime, _fake, client) = runtime_with_fake();
    (runtime, client)
}

/// Feed one mouse event, timed far enough from any other that no two presses
/// read as a double click.
///
/// The runtime tells a double click from two separate clicks by the gap between
/// them, so a test that pressed twice at the wall clock would double-click by
/// accident. Every event here is stamped an hour on, which no threshold reaches.
/// A test that wants a real double click drives
/// [`Server::handle_mouse_input`] with its own instants.
fn mouse(runtime: &mut Server, client: ClientId, input: MouseInput) {
    runtime.handle_mouse_input(client, input, far_apart());
}

/// An instant an hour after the last one this returned, so successive presses
/// never fall inside a click threshold.
fn far_apart() -> Instant {
    use std::sync::atomic::{AtomicU64, Ordering};
    static HOURS: AtomicU64 = AtomicU64::new(1);
    let hours = HOURS.fetch_add(1, Ordering::Relaxed);
    Instant::now() + Duration::from_secs(hours * 3600)
}

fn runtime_with_fake() -> (Server, Arc<FakePtyBackend>, ClientId) {
    let fake = Arc::new(FakePtyBackend::new());
    let (tx, rx) = mpsc::channel();
    let mut runtime = Server::new(
        fake.clone(),
        Arc::new(NullSnapshotProvider),
        Arc::new(NullStorage),
        rx,
        tx,
        Direction::Right,
    );
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size { cols: 80, rows: 24 },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    (runtime, fake, client)
}

/// The client's single bootstrap pane.
fn only_pane(runtime: &Server) -> PaneId {
    *runtime.pty_handles.keys().next().expect("one pane")
}

/// A screen cell inside `pane`'s content, with the 1-based pane-local column and
/// row a mouse report would carry for it.
fn a_content_cell(runtime: &Server, client: ClientId, pane: PaneId) -> (Point, u16, u16) {
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    let viewport = snapshot.client.viewport;
    for y in 0..viewport.rows {
        for x in 0..viewport.cols {
            let at = Point { x, y };
            if hit_test(&snapshot, at) == (HitRegion::PaneContent { pane_id: pane }) {
                let (col, row) = pane_local_cell(&snapshot, pane, at).expect("local cell");
                return (at, col, row);
            }
        }
    }
    panic!("no content cell for the pane");
}

fn press(x: u16, y: u16) -> MouseInput {
    MouseInput {
        kind: MouseKind::Press(MouseButton::Left),
        at: Point { x, y },
        mods: ModFlags::NONE,
    }
}

fn add_tab(runtime: &mut Server, client: ClientId) {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client),
        SystemTime::now(),
        Command::NewTab(NewTabArgs::default()),
    );
    let _ = runtime.dispatch(envelope);
}

/// The first cell on the tabline row whose hit region satisfies `pred`, scanning
/// from `min_x`.
fn find_on_tabline(
    runtime: &Server,
    client: ClientId,
    min_x: u16,
    pred: impl Fn(HitRegion) -> bool,
) -> u16 {
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    (min_x..snapshot.client.viewport.cols)
        .find(|&x| pred(hit_test(&snapshot, Point { x, y: 0 })))
        .expect("a matching tabline cell")
}

fn offset(runtime: &Server, client: ClientId) -> Option<usize> {
    runtime
        .build_snapshot(client)
        .expect("snapshot")
        .client
        .tabline_offset
}

#[test]
fn clicking_an_inactive_tab_focuses_it_and_clears_the_peek() {
    let (mut runtime, client) = runtime();
    add_tab(&mut runtime, client); // two tabs; the new one is active

    // Start a peek anchored at tab 0 so it stays on the strip regardless of how
    // wide the auto-generated session and tab names happen to render, then click
    // that (now inactive) tab.
    runtime
        .client_mut(client)
        .unwrap()
        .set_tabline_offset(Some(0));
    let snapshot = runtime.build_snapshot(client).unwrap();
    let first_tab = snapshot
        .session
        .tabs_metadata
        .iter()
        .find(|meta| meta.index == 0)
        .unwrap()
        .id;
    let x = find_on_tabline(&runtime, client, 0, |region| {
        region == HitRegion::Tab { tab_id: first_tab }
    });

    mouse(&mut runtime, client, press(x, 0));

    let snapshot = runtime.build_snapshot(client).unwrap();
    assert_eq!(
        snapshot.client.active_tab, first_tab,
        "clicked tab is active"
    );
    assert_eq!(
        snapshot.client.tabline_offset, None,
        "peek cleared on switch"
    );
}

#[test]
fn clicking_the_right_scroll_arrow_peeks_toward_the_end() {
    let (mut runtime, client) = runtime();
    for _ in 0..30 {
        add_tab(&mut runtime, client); // overflow the 80-column strip
    }
    runtime
        .client_mut(client)
        .unwrap()
        .set_tabline_offset(Some(0));

    let x = find_on_tabline(&runtime, client, 0, |region| {
        matches!(region, HitRegion::TablineScrollRight { .. })
    });
    let to = match hit_test(&runtime.build_snapshot(client).unwrap(), Point { x, y: 0 }) {
        HitRegion::TablineScrollRight { to } => to,
        other => panic!("expected a right scroll arrow, got {other:?}"),
    };

    mouse(&mut runtime, client, press(x, 0));

    assert!(to > 0, "the right arrow scrolls toward the end");
    assert_eq!(offset(&runtime, client), Some(to));
}

#[test]
fn wheel_over_the_tabline_steps_the_offset() {
    let (mut runtime, client) = runtime();
    for _ in 0..30 {
        add_tab(&mut runtime, client);
    }
    runtime
        .client_mut(client)
        .unwrap()
        .set_tabline_offset(Some(0));

    let x = find_on_tabline(&runtime, client, 0, |region| {
        matches!(
            region,
            HitRegion::Tab { .. } | HitRegion::TablineScrollRight { .. }
        )
    });
    let wheel = MouseInput {
        kind: MouseKind::Scroll(ScrollDirection::Down),
        at: Point { x, y: 0 },
        mods: ModFlags::NONE,
    };

    mouse(&mut runtime, client, wheel);

    assert_eq!(
        offset(&runtime, client),
        Some(1),
        "wheel down steps one tab"
    );
}

#[test]
fn a_wheel_off_the_tabline_row_does_not_scroll_it() {
    let (mut runtime, client) = runtime();
    for _ in 0..30 {
        add_tab(&mut runtime, client);
    }
    runtime
        .client_mut(client)
        .unwrap()
        .set_tabline_offset(Some(2));

    // Row 10 is pane content, not the tabline.
    let wheel = MouseInput {
        kind: MouseKind::Scroll(ScrollDirection::Down),
        at: Point { x: 40, y: 10 },
        mods: ModFlags::NONE,
    };
    mouse(&mut runtime, client, wheel);

    assert_eq!(
        offset(&runtime, client),
        Some(2),
        "offset unchanged off-row"
    );
}

#[test]
fn motion_and_non_left_buttons_leave_state_untouched() {
    let (mut runtime, client) = runtime();
    for _ in 0..30 {
        add_tab(&mut runtime, client);
    }
    runtime
        .client_mut(client)
        .unwrap()
        .set_tabline_offset(Some(2));

    // Buttonless motion over the tabline scrolls nothing and begins no drag.
    mouse(
        &mut runtime,
        client,
        MouseInput {
            kind: MouseKind::Motion,
            at: Point { x: 5, y: 0 },
            mods: ModFlags::NONE,
        },
    );
    // A right press over a tab is neither a focus nor a scroll.
    mouse(
        &mut runtime,
        client,
        MouseInput {
            kind: MouseKind::Press(MouseButton::Right),
            at: Point { x: 5, y: 0 },
            mods: ModFlags::NONE,
        },
    );

    assert_eq!(
        offset(&runtime, client),
        Some(2),
        "ignored events do not scroll"
    );
    assert!(
        runtime.client_mut(client).unwrap().tabline_drag().is_none(),
        "ignored events begin no drag"
    );
}

#[test]
fn pressing_a_bare_tabline_cell_begins_a_drag() {
    let (mut runtime, client) = runtime();
    for _ in 0..30 {
        add_tab(&mut runtime, client);
    }
    runtime
        .client_mut(client)
        .unwrap()
        .set_tabline_offset(Some(0));

    let x = find_on_tabline(&runtime, client, 10, |region| region == HitRegion::Tabline);
    mouse(&mut runtime, client, press(x, 0));

    let drag = runtime
        .client_mut(client)
        .unwrap()
        .tabline_drag()
        .expect("the press began a drag");
    assert_eq!(drag.anchor_x, x);
    assert_eq!(drag.anchor_first_visible, 0);
}

#[test]
fn dragging_scrolls_from_the_anchor_and_release_ends_it() {
    let (mut runtime, client) = runtime();
    for _ in 0..30 {
        add_tab(&mut runtime, client);
    }
    // Arm a drag anchored at column 40 with first-visible 2, as a press would.
    runtime
        .client_mut(client)
        .unwrap()
        .set_tabline_drag(Some(TablineDragState {
            anchor_x: 40,
            anchor_first_visible: 2,
        }));

    // Drag left by two steps' worth of cells: scroll two tabs toward the end.
    let x = 40 - 2 * TABLINE_DRAG_STEP as u16;
    mouse(
        &mut runtime,
        client,
        MouseInput {
            kind: MouseKind::Drag(MouseButton::Left),
            at: Point { x, y: 0 },
            mods: ModFlags::NONE,
        },
    );
    assert_eq!(offset(&runtime, client), Some(4), "two steps past anchor 2");

    // Release ends the drag, leaving the scrolled offset.
    mouse(
        &mut runtime,
        client,
        MouseInput {
            kind: MouseKind::Release(MouseButton::Left),
            at: Point { x, y: 0 },
            mods: ModFlags::NONE,
        },
    );
    assert!(
        runtime.client_mut(client).unwrap().tabline_drag().is_none(),
        "release ended the drag"
    );
    assert_eq!(
        offset(&runtime, client),
        Some(4),
        "offset stays after release"
    );
}

fn drag(x: u16, y: u16) -> MouseInput {
    MouseInput {
        kind: MouseKind::Drag(MouseButton::Left),
        at: Point { x, y },
        mods: ModFlags::NONE,
    }
}

fn release() -> MouseInput {
    MouseInput {
        kind: MouseKind::Release(MouseButton::Left),
        at: Point { x: 0, y: 0 },
        mods: ModFlags::NONE,
    }
}

/// Split the focused pane in the runtime's default direction (Right), leaving
/// the tab with two side-by-side panes and a vertical border between them.
fn split_focused(runtime: &mut Server, client: ClientId) {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client),
        SystemTime::now(),
        Command::NewPane(NewPaneArgs::default()),
    );
    let _ = runtime.dispatch(envelope);
}

/// The solved width, in columns, of `pane`'s box in `client`'s current frame.
fn pane_cols(runtime: &Server, client: ClientId, pane: PaneId) -> u16 {
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    snapshot
        .session
        .active_tab
        .layout_solved
        .iter()
        .find(|slot| slot.pane_id == pane)
        .expect("pane in layout")
        .rect
        .size
        .cols
}

/// A cell on the vertical divider between two side-by-side panes: the left/right
/// border nearest the horizontal center, so it is the shared divider rather than
/// the pane area's outer frame at either edge. Panics if the frame has no
/// vertical border.
fn find_vertical_border(runtime: &Server, client: ClientId) -> (Point, PaneId, Direction) {
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    let viewport = snapshot.client.viewport;
    let y = viewport.rows / 2;
    let center = viewport.cols / 2;
    let mut best: Option<(u16, PaneId, Direction)> = None;
    for x in 0..viewport.cols {
        if let HitRegion::PaneBorder { pane_id, side } = hit_test(&snapshot, Point { x, y }) {
            if matches!(side, Direction::Left | Direction::Right)
                && best.is_none_or(|(bx, ..)| center.abs_diff(x) < center.abs_diff(bx))
            {
                best = Some((x, pane_id, side));
            }
        }
    }
    let (x, pane, side) = best.expect("a vertical pane border in the frame");
    (Point { x, y }, pane, side)
}

/// The column `n` cells outward (the grow direction) from `x` for a border on
/// `side`: rightward for a right border, leftward for a left border.
fn outward_x(side: Direction, x: u16, n: u16) -> u16 {
    match side {
        Direction::Right => x + n,
        Direction::Left => x - n,
        other => panic!("expected a vertical border, got {other:?}"),
    }
}

/// The column `n` cells inward (the shrink direction) from `x` for a border on
/// `side`: leftward for a right border, rightward for a left border.
fn inward_x(side: Direction, x: u16, n: u16) -> u16 {
    match side {
        Direction::Right => x - n,
        Direction::Left => x + n,
        other => panic!("expected a vertical border, got {other:?}"),
    }
}

/// The far viewport edge on a border's outward side: a drag there grows the
/// grabbed pane by more than its neighbor can ever donate.
fn outward_edge_x(side: Direction, viewport_cols: u16) -> u16 {
    match side {
        Direction::Right => viewport_cols - 1,
        Direction::Left => 0,
        other => panic!("expected a vertical border, got {other:?}"),
    }
}

#[test]
fn resize_delta_grows_toward_each_border_and_ignores_the_other_axis() {
    let from = Point { x: 10, y: 10 };
    // Right border: pointer rightward grows, leftward shrinks.
    assert_eq!(
        resize_delta(Direction::Right, from, Point { x: 13, y: 10 }),
        3
    );
    assert_eq!(
        resize_delta(Direction::Right, from, Point { x: 8, y: 10 }),
        -2
    );
    // Left border: pointer leftward grows.
    assert_eq!(
        resize_delta(Direction::Left, from, Point { x: 7, y: 10 }),
        3
    );
    assert_eq!(
        resize_delta(Direction::Left, from, Point { x: 12, y: 10 }),
        -2
    );
    // Down border: pointer downward grows.
    assert_eq!(
        resize_delta(Direction::Down, from, Point { x: 10, y: 14 }),
        4
    );
    // Up border: pointer upward grows.
    assert_eq!(resize_delta(Direction::Up, from, Point { x: 10, y: 6 }), 4);
    // A left/right border ignores vertical motion.
    assert_eq!(
        resize_delta(Direction::Right, from, Point { x: 10, y: 20 }),
        0
    );
}

#[test]
fn dragging_a_vertical_border_resizes_the_grabbed_pane_live() {
    let (mut runtime, client) = runtime();
    split_focused(&mut runtime, client);

    let (cell, pane, side) = find_vertical_border(&runtime, client);
    let before = pane_cols(&runtime, client, pane);

    mouse(&mut runtime, client, press(cell.x, cell.y));
    mouse(
        &mut runtime,
        client,
        drag(outward_x(side, cell.x, 3), cell.y),
    );

    assert_eq!(
        pane_cols(&runtime, client, pane),
        before + 3,
        "the grabbed pane grew by the three cells dragged toward its border"
    );
}

#[test]
fn a_shrink_drag_tracks_the_pointer_cell_for_cell() {
    let (mut runtime, client) = runtime();
    split_focused(&mut runtime, client);

    let (cell, pane, side) = find_vertical_border(&runtime, client);
    let before = pane_cols(&runtime, client, pane);

    mouse(&mut runtime, client, press(cell.x, cell.y));

    // Drag three cells inward to shrink the pane.
    mouse(
        &mut runtime,
        client,
        drag(inward_x(side, cell.x, 3), cell.y),
    );
    assert_eq!(
        pane_cols(&runtime, client, pane),
        before - 3,
        "the grabbed pane shrank by the three cells dragged inward"
    );

    // One more cell inward from the new pointer position shrinks by exactly one
    // more: the anchor followed the pointer, so it is not a sudden jump.
    mouse(
        &mut runtime,
        client,
        drag(inward_x(side, cell.x, 4), cell.y),
    );
    assert_eq!(
        pane_cols(&runtime, client, pane),
        before - 4,
        "the second drag shrinks one cell, tracking the pointer"
    );
}

#[test]
fn a_release_ends_the_resize_drag_so_a_later_drag_does_nothing() {
    let (mut runtime, client) = runtime();
    split_focused(&mut runtime, client);

    let (cell, pane, side) = find_vertical_border(&runtime, client);
    mouse(&mut runtime, client, press(cell.x, cell.y));
    mouse(
        &mut runtime,
        client,
        drag(outward_x(side, cell.x, 2), cell.y),
    );
    let after_drag = pane_cols(&runtime, client, pane);

    mouse(&mut runtime, client, release());
    assert!(
        runtime
            .client_mut(client)
            .unwrap()
            .pending_resize_drag()
            .is_none(),
        "release cleared the resize drag"
    );

    // With no resize drag in progress, a stray drag resizes nothing.
    mouse(
        &mut runtime,
        client,
        drag(outward_x(side, cell.x, 6), cell.y),
    );
    assert_eq!(
        pane_cols(&runtime, client, pane),
        after_drag,
        "no resize drag is in progress, so the pointer move is ignored"
    );
}

#[test]
fn a_fast_over_drag_fills_to_the_wall_then_reverses_at_once() {
    let (mut runtime, client) = runtime();
    split_focused(&mut runtime, client);

    let (cell, pane, side) = find_vertical_border(&runtime, client);
    let before = pane_cols(&runtime, client, pane);
    let viewport_cols = runtime.build_snapshot(client).unwrap().client.viewport.cols;

    mouse(&mut runtime, client, press(cell.x, cell.y));

    // One big jump past the wall: the drag is applied a cell at a time, so it
    // grows the pane as far as the neighbor can donate instead of refusing the
    // whole move.
    mouse(
        &mut runtime,
        client,
        drag(outward_edge_x(side, viewport_cols), cell.y),
    );
    let grown = pane_cols(&runtime, client, pane);
    assert!(
        grown > before,
        "the jump grew the pane toward the neighbor's minimum ({before} -> {grown})"
    );

    // Pointer still further out: the neighbor is already at its minimum, so the
    // anchor sits at the wall and nothing more moves.
    mouse(
        &mut runtime,
        client,
        drag(outward_edge_x(side, viewport_cols), cell.y),
    );
    assert_eq!(
        pane_cols(&runtime, client, pane),
        grown,
        "held at the wall while the pointer overshoots"
    );

    // Reverse straight back to the original border cell: the anchor held at the
    // wall, so the pane shrinks back with no dead zone.
    mouse(&mut runtime, client, drag(cell.x, cell.y));
    assert_eq!(
        pane_cols(&runtime, client, pane),
        before,
        "a reverse drag returns the border to where it started, no lag"
    );
}

#[test]
fn advance_toward_moves_the_anchor_toward_the_pointer_and_saturates() {
    let from = Point { x: 3, y: 3 };
    // Steps toward the pointer's coordinate on the border's own axis.
    assert_eq!(
        advance_toward(Direction::Right, from, Point { x: 9, y: 3 }, 2),
        Point { x: 5, y: 3 }
    );
    assert_eq!(
        advance_toward(Direction::Left, from, Point { x: 0, y: 3 }, 2),
        Point { x: 1, y: 3 }
    );
    assert_eq!(
        advance_toward(Direction::Down, from, Point { x: 3, y: 9 }, 2),
        Point { x: 3, y: 5 }
    );
    assert_eq!(
        advance_toward(Direction::Up, from, Point { x: 3, y: 0 }, 2),
        Point { x: 3, y: 1 }
    );
    // A left/right border reads only x; the pointer's y is ignored.
    assert_eq!(
        advance_toward(Direction::Right, from, Point { x: 9, y: 99 }, 2),
        Point { x: 5, y: 3 }
    );
    // Saturating: an anchor near an edge cannot wrap below zero.
    assert_eq!(
        advance_toward(Direction::Left, from, Point { x: 0, y: 3 }, 10),
        Point { x: 0, y: 3 }
    );
    assert_eq!(
        advance_toward(Direction::Up, from, Point { x: 3, y: 0 }, 10),
        Point { x: 3, y: 0 }
    );
}

/// Split the focused pane downward, leaving the tab with a top and bottom pane
/// and a horizontal border between them.
fn split_focused_vertical(runtime: &mut Server, client: ClientId) {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client),
        SystemTime::now(),
        Command::NewPane(NewPaneArgs {
            direction: Some(Direction::Down),
            ..NewPaneArgs::default()
        }),
    );
    let _ = runtime.dispatch(envelope);
}

/// The solved height, in rows, of `pane`'s box in `client`'s current frame.
fn pane_rows(runtime: &Server, client: ClientId, pane: PaneId) -> u16 {
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    snapshot
        .session
        .active_tab
        .layout_solved
        .iter()
        .find(|slot| slot.pane_id == pane)
        .expect("pane in layout")
        .rect
        .size
        .rows
}

/// A cell on the horizontal divider between a top and bottom pane: the up/down
/// border nearest the vertical center, so it is the shared divider rather than
/// the outer frame. Panics if the frame has no horizontal border.
fn find_horizontal_border(runtime: &Server, client: ClientId) -> (Point, PaneId, Direction) {
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    let viewport = snapshot.client.viewport;
    let x = viewport.cols / 2;
    let center = viewport.rows / 2;
    let mut best: Option<(u16, PaneId, Direction)> = None;
    for y in 1..viewport.rows - 1 {
        if let HitRegion::PaneBorder { pane_id, side } = hit_test(&snapshot, Point { x, y }) {
            if matches!(side, Direction::Up | Direction::Down)
                && best.is_none_or(|(by, ..)| center.abs_diff(y) < center.abs_diff(by))
            {
                best = Some((y, pane_id, side));
            }
        }
    }
    let (y, pane, side) = best.expect("a horizontal pane border in the frame");
    (Point { x, y }, pane, side)
}

/// The row `n` cells outward (the grow direction) from `y` for a border on
/// `side`: downward for a down border, upward for an up border.
fn outward_y(side: Direction, y: u16, n: u16) -> u16 {
    match side {
        Direction::Down => y + n,
        Direction::Up => y - n,
        other => panic!("expected a horizontal border, got {other:?}"),
    }
}

/// The rightmost vertical border in the frame: the pane area's outer right
/// frame, which has no neighbor on its outward side.
fn find_outer_vertical_frame(runtime: &Server, client: ClientId) -> (Point, PaneId, Direction) {
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    let viewport = snapshot.client.viewport;
    let y = viewport.rows / 2;
    let mut best: Option<(u16, PaneId, Direction)> = None;
    for x in 0..viewport.cols {
        if let HitRegion::PaneBorder { pane_id, side } = hit_test(&snapshot, Point { x, y }) {
            if matches!(side, Direction::Left | Direction::Right)
                && best.is_none_or(|(bx, ..)| x > bx)
            {
                best = Some((x, pane_id, side));
            }
        }
    }
    let (x, pane, side) = best.expect("a vertical pane border in the frame");
    (Point { x, y }, pane, side)
}

#[test]
fn dragging_a_horizontal_border_resizes_the_grabbed_pane_live() {
    let (mut runtime, client) = runtime();
    split_focused_vertical(&mut runtime, client);

    let (cell, pane, side) = find_horizontal_border(&runtime, client);
    let before = pane_rows(&runtime, client, pane);

    mouse(&mut runtime, client, press(cell.x, cell.y));
    mouse(
        &mut runtime,
        client,
        drag(cell.x, outward_y(side, cell.y, 3)),
    );

    assert_eq!(
        pane_rows(&runtime, client, pane),
        before + 3,
        "the grabbed pane grew by the three rows dragged toward its border"
    );
}

#[test]
fn grabbing_the_outer_frame_starts_no_resize() {
    let (mut runtime, client) = runtime();
    split_focused(&mut runtime, client);

    let (cell, pane, side) = find_outer_vertical_frame(&runtime, client);
    assert_eq!(
        side,
        Direction::Right,
        "the rightmost frame is a right border"
    );
    let before = pane_cols(&runtime, client, pane);

    // The outer frame sits at the tab edge and cannot move, so grabbing it starts
    // no resize drag.
    mouse(&mut runtime, client, press(cell.x, cell.y));
    assert!(
        runtime
            .client_mut(client)
            .unwrap()
            .pending_resize_drag()
            .is_none(),
        "the outer frame is not draggable, so no resize drag begins"
    );

    // A drag inward after that changes nothing either.
    mouse(&mut runtime, client, drag(cell.x - 3, cell.y));
    assert_eq!(
        pane_cols(&runtime, client, pane),
        before,
        "grabbing the terminal's outer edge resizes nothing"
    );
}

#[test]
fn grabbing_the_frame_of_a_fullscreen_pane_starts_no_resize() {
    let (mut runtime, client) = runtime();
    split_focused(&mut runtime, client);
    let (_, pane, _) = find_vertical_border(&runtime, client);
    let tiled_cols = pane_cols(&runtime, client, pane);

    // Zoom the focused pane: its border ring is now the outer frame, while the
    // tiled tree underneath still has a hidden neighbor to its side.
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client),
        SystemTime::now(),
        Command::TogglePaneFullscreen,
    );
    let _ = runtime.dispatch(envelope);
    let active_tab = runtime.client_mut(client).unwrap().active_tab();

    // Grab the zoomed pane's right frame edge and drag inward: no divider is
    // visible under a zoom, so no resize begins, the zoom stands, and the
    // hidden tiled layout is untouched.
    let (cell, _, _) = find_vertical_border(&runtime, client);
    mouse(&mut runtime, client, press(cell.x, cell.y));
    assert!(
        runtime
            .client_mut(client)
            .unwrap()
            .pending_resize_drag()
            .is_none(),
        "a zoomed view has no draggable border, so no resize drag begins"
    );

    mouse(&mut runtime, client, drag(cell.x - 3, cell.y));
    assert!(
        matches!(
            runtime.client_mut(client).unwrap().layout_mode(active_tab),
            LayoutMode::Fullscreen { .. }
        ),
        "no resize was dispatched, so the client's zoom stands"
    );

    // Toggle back out: the tiled layout is exactly as it was.
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client),
        SystemTime::now(),
        Command::TogglePaneFullscreen,
    );
    let _ = runtime.dispatch(envelope);
    assert_eq!(
        pane_cols(&runtime, client, pane),
        tiled_cols,
        "the hidden tiled layout was not mutated by the drag"
    );
}

#[test]
fn a_click_in_the_focused_pane_forwards_a_report_when_the_program_asks() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    // The program turns on normal tracking with SGR encoding.
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    let (at, col, row) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, client, press(at.x, at.y));

    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![format!("\x1b[<0;{col};{row}M").into_bytes()],
        "the click in the focused pane is forwarded as an SGR report"
    );
}

#[test]
fn a_click_forwards_nothing_when_the_program_wants_no_mouse() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, client, press(at.x, at.y));

    assert_eq!(
        fake.writes(pane).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "a pane in no mouse mode receives nothing"
    );
}

#[test]
fn a_press_drag_release_gesture_forwards_each_event() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    // Button-event tracking reports drags; SGR encoding.
    runtime.handle_pty_output(pane, b"\x1b[?1002h\x1b[?1006h");
    let (at, col, row) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, client, press(at.x, at.y));
    mouse(&mut runtime, client, drag(at.x, at.y));
    mouse(
        &mut runtime,
        client,
        MouseInput {
            kind: MouseKind::Release(MouseButton::Left),
            at,
            mods: ModFlags::NONE,
        },
    );

    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![
            format!("\x1b[<0;{col};{row}M").into_bytes(),
            format!("\x1b[<32;{col};{row}M").into_bytes(),
            format!("\x1b[<0;{col};{row}m").into_bytes(),
        ],
        "press, then drag with the motion bit, then release with a lowercase m"
    );
}

#[test]
fn a_mouse_select_gesture_over_a_mouse_aware_program_forwards_nothing() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    // Button-event tracking would report a bare drag; SGR encoding.
    runtime.handle_pty_output(pane, b"\x1b[?1002h\x1b[?1006h");
    // Grab the mouse for koshi selection.
    runtime
        .client_mut(client)
        .expect("client")
        .toggle_mouse_select();
    let (at, _, _) = a_content_cell(&runtime, client, pane);
    let gesture = |kind| MouseInput {
        kind,
        at,
        mods: ModFlags::NONE,
    };

    mouse(
        &mut runtime,
        client,
        gesture(MouseKind::Press(MouseButton::Left)),
    );
    mouse(
        &mut runtime,
        client,
        gesture(MouseKind::Drag(MouseButton::Left)),
    );
    mouse(
        &mut runtime,
        client,
        gesture(MouseKind::Release(MouseButton::Left)),
    );

    assert_eq!(
        fake.writes(pane).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "in mouse-select mode the gesture is koshi's selection; the program is sent nothing"
    );
}

#[test]
fn a_bare_move_forwards_only_in_any_motion_mode() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    let (at, col, row) = a_content_cell(&runtime, client, pane);
    let motion = MouseInput {
        kind: MouseKind::Motion,
        at,
        mods: ModFlags::NONE,
    };

    // Normal tracking does not report motion: the move forwards nothing (and the
    // frame is never rebuilt to check).
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    mouse(&mut runtime, client, motion);
    assert_eq!(
        fake.writes(pane).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "normal tracking ignores a bare move"
    );

    // Any-motion tracking reports it: no-button 3 + motion bit 32 = 35.
    runtime.handle_pty_output(pane, b"\x1b[?1003h");
    mouse(&mut runtime, client, motion);
    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![format!("\x1b[<35;{col};{row}M").into_bytes()],
        "any-motion tracking reports the move"
    );
}

#[test]
fn a_captured_release_is_re_stamped_to_the_pressed_button() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    let (at, col, row) = a_content_cell(&runtime, client, pane);

    // A right press captures the gesture (button 2).
    mouse(
        &mut runtime,
        client,
        MouseInput {
            kind: MouseKind::Press(MouseButton::Right),
            at,
            mods: ModFlags::NONE,
        },
    );
    // The terminal reports the release as the left button (a stand-in); it must
    // still reach the program as a right release, matching the press.
    mouse(
        &mut runtime,
        client,
        MouseInput {
            kind: MouseKind::Release(MouseButton::Left),
            at,
            mods: ModFlags::NONE,
        },
    );

    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![
            format!("\x1b[<2;{col};{row}M").into_bytes(),
            format!("\x1b[<2;{col};{row}m").into_bytes(),
        ],
        "the release re-stamps to button 2, not the reported left button 0"
    );
}

#[test]
fn a_drag_with_no_captured_press_is_dropped() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1002h\x1b[?1006h");
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    // A drag arrives without a press to capture the gesture (a release with no
    // matching press is the orphan-release case) — nothing is forwarded.
    mouse(&mut runtime, client, drag(at.x, at.y));
    mouse(
        &mut runtime,
        client,
        MouseInput {
            kind: MouseKind::Release(MouseButton::Left),
            at,
            mods: ModFlags::NONE,
        },
    );

    assert_eq!(
        fake.writes(pane).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "a gesture with no captured press forwards nothing"
    );
}

#[test]
fn a_captured_drag_that_leaves_the_pane_clamps_to_its_edge() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1002h\x1b[?1006h");
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    // Press inside the pane to capture the gesture, then drag far past its top-
    // left corner (0, 0 is the tabline row, outside the pane); the captured drag
    // clamps to the pane's first cell.
    mouse(&mut runtime, client, press(at.x, at.y));
    mouse(&mut runtime, client, drag(0, 0));

    assert_eq!(
        fake.writes(pane).expect("writes").last().expect("a drag"),
        &b"\x1b[<32;1;1M".to_vec(),
        "the drag clamps to the pane's top-left cell (1, 1)"
    );
}

#[test]
fn border_resize_off_leaves_a_border_press_inert() {
    let (mut runtime, client) = runtime();
    runtime.config.mouse.border_resize = false;
    split_focused(&mut runtime, client);

    let (cell, pane, side) = find_vertical_border(&runtime, client);
    let before = pane_cols(&runtime, client, pane);

    mouse(&mut runtime, client, press(cell.x, cell.y));
    mouse(
        &mut runtime,
        client,
        drag(outward_x(side, cell.x, 3), cell.y),
    );

    assert_eq!(
        pane_cols(&runtime, client, pane),
        before,
        "with border resize disabled, a border drag changes nothing"
    );
}

#[test]
fn a_click_on_an_unfocused_pane_focuses_it_rather_than_forwarding() {
    let (mut runtime, fake, client) = runtime_with_fake();
    split_focused(&mut runtime, client);
    let focused = runtime.typed_pane(client).expect("a focused pane");

    // The other pane in the split is not focused; both had mouse mode on, so a
    // forward would have written bytes.
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    let other = snapshot
        .session
        .active_tab
        .layout_solved
        .iter()
        .map(|slot| slot.pane_id)
        .find(|&id| id != focused)
        .expect("a second pane");
    runtime.handle_pty_output(other, b"\x1b[?1000h\x1b[?1006h");
    let (at, _, _) = a_content_cell(&runtime, client, other);

    mouse(&mut runtime, client, press(at.x, at.y));

    assert_eq!(
        runtime.typed_pane(client),
        Some(other),
        "the click moved focus"
    );
    assert_eq!(
        fake.writes(other).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "the first click only focuses; it is not forwarded"
    );
}

/// A wheel event at a screen cell.
fn wheel(direction: ScrollDirection, at: Point) -> MouseInput {
    MouseInput {
        kind: MouseKind::Scroll(direction),
        at,
        mods: ModFlags::NONE,
    }
}

/// The client's scrollback view offset for a pane.
fn scroll_offset(runtime: &Server, client: ClientId, pane: PaneId) -> usize {
    runtime
        .sessions()
        .values()
        .next()
        .unwrap()
        .clients
        .get(client)
        .unwrap()
        .scroll_offset(pane)
}

/// Whether the client has a highlight up in the pane.
fn has_highlight(runtime: &Server, client: ClientId, pane: PaneId) -> bool {
    runtime
        .sessions()
        .values()
        .next()
        .unwrap()
        .clients
        .get(client)
        .unwrap()
        .selection(pane)
        .is_some()
}

/// The pane the client's pointer is marked as hovering over.
fn hovered(runtime: &Server, client: ClientId) -> Option<PaneId> {
    runtime
        .build_snapshot(client)
        .expect("snapshot")
        .client
        .hovered_pane
}

/// Fill a pane's scrollback with `lines` lines by printing that many newlines,
/// so a scroll up has room to move.
fn feed_scrollback(runtime: &mut Server, pane: PaneId, lines: usize) {
    for _ in 0..lines {
        runtime.handle_pty_output(pane, b"x\r\n");
    }
}

/// Put a highlight in the pane, as a drag would, so the view is held.
fn set_highlight(runtime: &mut Server, client: ClientId, pane: PaneId) {
    runtime.client_mut(client).unwrap().set_selection(
        pane,
        Selection {
            kind: SelectionKind::Character,
            anchor: GridPos { row: 0, col: 0 },
            cursor: GridPos { row: 0, col: 4 },
        },
    );
}

#[test]
fn a_wheel_over_a_plain_pane_scrolls_its_scrollback() {
    let (mut runtime, client) = runtime();
    let pane = only_pane(&runtime);
    feed_scrollback(&mut runtime, pane, 40);
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, client, wheel(ScrollDirection::Up, at));

    // scroll_lines defaults to 3, so one wheel up moves the view three lines.
    assert_eq!(scroll_offset(&runtime, client, pane), 3, "wheel up scrolls");
    assert_eq!(
        offset(&runtime, client),
        None,
        "the pane wheel leaves the tab strip alone"
    );
}

#[test]
fn a_wheel_down_returns_the_view_toward_live() {
    let (mut runtime, client) = runtime();
    let pane = only_pane(&runtime);
    feed_scrollback(&mut runtime, pane, 40);
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, client, wheel(ScrollDirection::Up, at));
    mouse(&mut runtime, client, wheel(ScrollDirection::Up, at));
    assert_eq!(
        scroll_offset(&runtime, client, pane),
        6,
        "two ups, six lines"
    );

    mouse(&mut runtime, client, wheel(ScrollDirection::Down, at));
    assert_eq!(
        scroll_offset(&runtime, client, pane),
        3,
        "a wheel down walks the view back three lines"
    );
}

#[test]
fn a_wheel_with_a_highlight_up_scrolls_and_keeps_the_highlight() {
    let (mut runtime, client) = runtime();
    let pane = only_pane(&runtime);
    feed_scrollback(&mut runtime, pane, 40);
    set_highlight(&mut runtime, client, pane);
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, client, wheel(ScrollDirection::Up, at));

    assert_eq!(
        scroll_offset(&runtime, client, pane),
        3,
        "a highlighted view still scrolls on the wheel"
    );
    assert!(
        has_highlight(&runtime, client, pane),
        "the wheel holds the highlight; it does not clear it"
    );
}

#[test]
fn a_wheel_over_a_mouse_reporting_pane_forwards_a_report() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    feed_scrollback(&mut runtime, pane, 40);
    // The program turns on normal tracking with SGR encoding.
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    let (at, col, row) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, client, wheel(ScrollDirection::Up, at));

    // Wheel up is SGR button 64; the program gets it, and koshi does not scroll.
    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![format!("\x1b[<64;{col};{row}M").into_bytes()],
        "the wheel is forwarded as a mouse report"
    );
    assert_eq!(
        scroll_offset(&runtime, client, pane),
        0,
        "a mouse-reporting pane keeps its own scrollback still"
    );
}

#[test]
fn a_wheel_on_the_alternate_screen_with_alt_scroll_sends_arrow_keys() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    // Enter the alternate screen and turn alternate-scroll on, with no mouse mode.
    runtime.handle_pty_output(pane, b"\x1b[?1049h\x1b[?1007h");
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, client, wheel(ScrollDirection::Up, at));
    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![b"\x1b[A\x1b[A\x1b[A".to_vec()],
        "wheel up becomes three up-arrows under default cursor keys"
    );

    mouse(&mut runtime, client, wheel(ScrollDirection::Down, at));
    assert_eq!(
        fake.writes(pane).expect("writes").last().expect("a write"),
        &b"\x1b[B\x1b[B\x1b[B".to_vec(),
        "wheel down becomes three down-arrows"
    );
}

#[test]
fn alt_scroll_uses_application_cursor_keys_when_the_program_asks() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    // Alternate screen, alternate-scroll on, application cursor keys on.
    runtime.handle_pty_output(pane, b"\x1b[?1049h\x1b[?1007h\x1b[?1h");
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, client, wheel(ScrollDirection::Up, at));

    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![b"\x1bOA\x1bOA\x1bOA".to_vec()],
        "application cursor keys send the SS3 form ESC O A"
    );
}

#[test]
fn the_ignore_wheel_config_does_nothing_over_a_plain_pane() {
    let (mut runtime, fake, client) = runtime_with_fake();
    runtime.config.mouse.wheel = WheelScroll::Ignore;
    let pane = only_pane(&runtime);
    feed_scrollback(&mut runtime, pane, 40);
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, client, wheel(ScrollDirection::Up, at));

    assert_eq!(
        scroll_offset(&runtime, client, pane),
        0,
        "the ignore setting leaves the view where it is"
    );
    assert_eq!(
        fake.writes(pane).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "the ignore setting forwards nothing either"
    );
}

#[test]
fn a_horizontal_wheel_does_not_scroll_the_scrollback() {
    let (mut runtime, client) = runtime();
    let pane = only_pane(&runtime);
    feed_scrollback(&mut runtime, pane, 40);
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, client, wheel(ScrollDirection::Left, at));

    assert_eq!(
        scroll_offset(&runtime, client, pane),
        0,
        "a horizontal wheel leaves the vertical scrollback view alone"
    );
}

#[test]
fn a_move_marks_the_hovered_pane_and_clears_it_off_a_pane() {
    let (mut runtime, client) = runtime();
    let pane = only_pane(&runtime);
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    mouse(
        &mut runtime,
        client,
        MouseInput {
            kind: MouseKind::Motion,
            at,
            mods: ModFlags::NONE,
        },
    );
    assert_eq!(
        hovered(&runtime, client),
        Some(pane),
        "a move over pane content marks it hovered"
    );

    // Row 0 is the tabline, not a pane.
    mouse(
        &mut runtime,
        client,
        MouseInput {
            kind: MouseKind::Motion,
            at: Point { x: 0, y: 0 },
            mods: ModFlags::NONE,
        },
    );
    assert_eq!(
        hovered(&runtime, client),
        None,
        "a move onto chrome clears the hover"
    );
}

#[test]
fn a_wheel_scrolls_the_pane_under_the_pointer_not_the_focused_one() {
    let (mut runtime, client) = runtime();
    split_focused(&mut runtime, client);
    let focused = runtime.typed_pane(client).expect("a focused pane");
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    let other = snapshot
        .session
        .active_tab
        .layout_solved
        .iter()
        .map(|slot| slot.pane_id)
        .find(|&id| id != focused)
        .expect("a second pane");

    feed_scrollback(&mut runtime, other, 40);
    let (at, _, _) = a_content_cell(&runtime, client, other);

    mouse(&mut runtime, client, wheel(ScrollDirection::Up, at));

    assert_eq!(
        scroll_offset(&runtime, client, other),
        3,
        "the pane under the pointer scrolls"
    );
    assert_eq!(
        scroll_offset(&runtime, client, focused),
        0,
        "the focused pane is left alone"
    );
}

#[test]
fn a_wheel_over_a_pane_border_scrolls_the_focused_pane() {
    let (mut runtime, client) = runtime();
    split_focused(&mut runtime, client);
    let focused = runtime.typed_pane(client).expect("a focused pane");
    feed_scrollback(&mut runtime, focused, 40);

    // The divider between the two panes is chrome, not pane content: a wheel
    // there has no pane under the pointer, so it falls to the focused pane.
    let (cell, _, _) = find_vertical_border(&runtime, client);
    mouse(&mut runtime, client, wheel(ScrollDirection::Up, cell));

    assert_eq!(
        scroll_offset(&runtime, client, focused),
        3,
        "a wheel over chrome scrolls the focused pane"
    );
}

#[test]
fn a_wheel_over_an_unfocused_mouse_app_forwards_to_that_pane() {
    let (mut runtime, fake, client) = runtime_with_fake();
    split_focused(&mut runtime, client);
    let focused = runtime.typed_pane(client).expect("a focused pane");
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    let other = snapshot
        .session
        .active_tab
        .layout_solved
        .iter()
        .map(|slot| slot.pane_id)
        .find(|&id| id != focused)
        .expect("a second pane");

    // The unfocused pane's program wants the mouse: normal tracking, SGR.
    runtime.handle_pty_output(other, b"\x1b[?1000h\x1b[?1006h");
    let (at, col, row) = a_content_cell(&runtime, client, other);

    mouse(&mut runtime, client, wheel(ScrollDirection::Up, at));

    // Wheel up is SGR button 64; it reaches the pane under the pointer even
    // though that pane is unfocused, and the focused pane gets nothing.
    assert_eq!(
        fake.writes(other).expect("writes"),
        vec![format!("\x1b[<64;{col};{row}M").into_bytes()],
        "the wheel forwards to the unfocused pane under the pointer"
    );
    assert_eq!(
        fake.writes(focused).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "the focused pane receives nothing"
    );
}

#[test]
fn a_wheel_on_the_alternate_screen_without_alt_scroll_stores_no_offset() {
    let (mut runtime, client) = runtime();
    let pane = only_pane(&runtime);
    feed_scrollback(&mut runtime, pane, 40);
    // Enter the alternate screen with neither mouse mode nor alt-scroll (?1007):
    // a full-screen app that ignores the wheel.
    runtime.handle_pty_output(pane, b"\x1b[?1049h");
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, client, wheel(ScrollDirection::Up, at));

    // The alternate screen keeps no scrollback, so the wheel stores no offset —
    // otherwise the shell would be scrolled back when the app exits.
    assert_eq!(
        scroll_offset(&runtime, client, pane),
        0,
        "a wheel on the alternate screen leaves the primary offset at 0"
    );
}

/// A screen cell that is chrome, not any pane's content — a pane border, the
/// status line, or a gap — where a wheel falls through to the focused pane.
fn a_chrome_cell(runtime: &Server, client: ClientId) -> Point {
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    let viewport = snapshot.client.viewport;
    for y in 0..viewport.rows {
        for x in 0..viewport.cols {
            let at = Point { x, y };
            if matches!(
                hit_test(&snapshot, at),
                HitRegion::PaneBorder { .. } | HitRegion::Statusline | HitRegion::None
            ) {
                return at;
            }
        }
    }
    panic!("no chrome cell in the frame");
}

#[test]
fn a_wheel_over_chrome_reaches_the_focused_mouse_app() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    // The focused pane's program wants the mouse: normal tracking, SGR.
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");

    // A wheel over chrome (no pane under the pointer) goes to the focused pane,
    // clamped to its edge, instead of being dropped.
    let chrome = a_chrome_cell(&runtime, client);
    mouse(&mut runtime, client, wheel(ScrollDirection::Up, chrome));

    let writes = fake.writes(pane).expect("writes");
    assert_eq!(
        writes.len(),
        1,
        "the wheel reached the focused pane: {writes:?}"
    );
    assert!(
        writes[0].starts_with(b"\x1b[<64;"),
        "an SGR wheel-up report (button 64): {:?}",
        writes[0]
    );
}
