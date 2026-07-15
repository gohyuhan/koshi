//! Mouse routing tests: a tab click focuses the tab and clears the peek, a
//! scroll arrow and the wheel peek the strip, and a tabline drag scrolls it.
//!
//! Client state is read back through [`Runtime::build_snapshot`] — the same
//! projection the renderer draws — so a test never reaches into private client
//! fields.

use super::*;

use std::sync::{mpsc, Arc};

use koshi_core::command::{NewPaneArgs, NewTabArgs};
use koshi_core::geometry::{Direction, Size};
use koshi_core::key::ModFlags;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::hit_test;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage};

fn runtime() -> (Runtime, ClientId) {
    let fake = Arc::new(FakePtyBackend::new());
    let (tx, rx) = mpsc::channel();
    let mut runtime = Runtime::new(
        fake,
        Arc::new(NullSnapshotProvider),
        Arc::new(NullStorage),
        rx,
        tx,
        TerminalCleanupGuard::new(),
        Direction::Right,
    );
    let client = runtime
        .bootstrap_local(Size { cols: 80, rows: 24 }, SystemTime::UNIX_EPOCH)
        .expect("bootstrap");
    (runtime, client)
}

fn press(x: u16, y: u16) -> MouseInput {
    MouseInput {
        kind: MouseKind::Press(MouseButton::Left),
        at: Point { x, y },
        mods: ModFlags::NONE,
    }
}

fn add_tab(runtime: &mut Runtime, client: ClientId) {
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
    runtime: &Runtime,
    client: ClientId,
    min_x: u16,
    pred: impl Fn(HitRegion) -> bool,
) -> u16 {
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    (min_x..snapshot.client.viewport.cols)
        .find(|&x| pred(hit_test(&snapshot, Point { x, y: 0 })))
        .expect("a matching tabline cell")
}

fn offset(runtime: &Runtime, client: ClientId) -> Option<usize> {
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

    // Peek somewhere, then click the first (now inactive) tab.
    runtime
        .client_mut(client)
        .unwrap()
        .set_tabline_offset(Some(1));
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

    runtime.handle_mouse_input(client, press(x, 0));

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

    runtime.handle_mouse_input(client, press(x, 0));

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

    runtime.handle_mouse_input(client, wheel);

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
    runtime.handle_mouse_input(client, wheel);

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
    runtime.handle_mouse_input(
        client,
        MouseInput {
            kind: MouseKind::Motion,
            at: Point { x: 5, y: 0 },
            mods: ModFlags::NONE,
        },
    );
    // A right press over a tab is neither a focus nor a scroll.
    runtime.handle_mouse_input(
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
    runtime.handle_mouse_input(client, press(x, 0));

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
    runtime.handle_mouse_input(
        client,
        MouseInput {
            kind: MouseKind::Drag(MouseButton::Left),
            at: Point { x, y: 0 },
            mods: ModFlags::NONE,
        },
    );
    assert_eq!(offset(&runtime, client), Some(4), "two steps past anchor 2");

    // Release ends the drag, leaving the scrolled offset.
    runtime.handle_mouse_input(
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
fn split_focused(runtime: &mut Runtime, client: ClientId) {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client),
        SystemTime::now(),
        Command::NewPane(NewPaneArgs::default()),
    );
    let _ = runtime.dispatch(envelope);
}

/// The solved width, in columns, of `pane`'s box in `client`'s current frame.
fn pane_cols(runtime: &Runtime, client: ClientId, pane: PaneId) -> u16 {
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
fn find_vertical_border(runtime: &Runtime, client: ClientId) -> (Point, PaneId, Direction) {
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

    runtime.handle_mouse_input(client, press(cell.x, cell.y));
    runtime.handle_mouse_input(client, drag(outward_x(side, cell.x, 3), cell.y));

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

    runtime.handle_mouse_input(client, press(cell.x, cell.y));

    // Drag three cells inward to shrink the pane.
    runtime.handle_mouse_input(client, drag(inward_x(side, cell.x, 3), cell.y));
    assert_eq!(
        pane_cols(&runtime, client, pane),
        before - 3,
        "the grabbed pane shrank by the three cells dragged inward"
    );

    // One more cell inward from the new pointer position shrinks by exactly one
    // more: the anchor followed the pointer, so it is not a sudden jump.
    runtime.handle_mouse_input(client, drag(inward_x(side, cell.x, 4), cell.y));
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
    runtime.handle_mouse_input(client, press(cell.x, cell.y));
    runtime.handle_mouse_input(client, drag(outward_x(side, cell.x, 2), cell.y));
    let after_drag = pane_cols(&runtime, client, pane);

    runtime.handle_mouse_input(client, release());
    assert!(
        runtime
            .client_mut(client)
            .unwrap()
            .pending_resize_drag()
            .is_none(),
        "release cleared the resize drag"
    );

    // With no resize drag in progress, a stray drag resizes nothing.
    runtime.handle_mouse_input(client, drag(outward_x(side, cell.x, 6), cell.y));
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

    runtime.handle_mouse_input(client, press(cell.x, cell.y));

    // One big jump past the wall: the drag is applied a cell at a time, so it
    // grows the pane as far as the neighbor can donate instead of refusing the
    // whole move.
    runtime.handle_mouse_input(client, drag(outward_edge_x(side, viewport_cols), cell.y));
    let grown = pane_cols(&runtime, client, pane);
    assert!(
        grown > before,
        "the jump grew the pane toward the neighbor's minimum ({before} -> {grown})"
    );

    // Pointer still further out: the neighbor is already at its minimum, so the
    // anchor sits at the wall and nothing more moves.
    runtime.handle_mouse_input(client, drag(outward_edge_x(side, viewport_cols), cell.y));
    assert_eq!(
        pane_cols(&runtime, client, pane),
        grown,
        "held at the wall while the pointer overshoots"
    );

    // Reverse straight back to the original border cell: the anchor held at the
    // wall, so the pane shrinks back with no dead zone.
    runtime.handle_mouse_input(client, drag(cell.x, cell.y));
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
fn split_focused_vertical(runtime: &mut Runtime, client: ClientId) {
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
fn pane_rows(runtime: &Runtime, client: ClientId, pane: PaneId) -> u16 {
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
fn find_horizontal_border(runtime: &Runtime, client: ClientId) -> (Point, PaneId, Direction) {
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
fn find_outer_vertical_frame(runtime: &Runtime, client: ClientId) -> (Point, PaneId, Direction) {
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

    runtime.handle_mouse_input(client, press(cell.x, cell.y));
    runtime.handle_mouse_input(client, drag(cell.x, outward_y(side, cell.y, 3)));

    assert_eq!(
        pane_rows(&runtime, client, pane),
        before + 3,
        "the grabbed pane grew by the three rows dragged toward its border"
    );
}

#[test]
fn dragging_the_outer_frame_resizes_via_fallback_without_panicking() {
    let (mut runtime, client) = runtime();
    split_focused(&mut runtime, client);

    let (cell, pane, side) = find_outer_vertical_frame(&runtime, client);
    assert_eq!(
        side,
        Direction::Right,
        "the rightmost frame is a right border"
    );
    let before = pane_cols(&runtime, client, pane);

    runtime.handle_mouse_input(client, press(cell.x, cell.y));
    // The outer frame has no neighbor on its outward side, so each step falls
    // back to the opposite border. Dragging it off-screen must not panic.
    runtime.handle_mouse_input(client, drag(cell.x + 20, cell.y));

    assert!(
        pane_cols(&runtime, client, pane) <= before,
        "the fallback can only shrink the grabbed pane here, never grow it"
    );
}
