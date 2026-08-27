//! Mouse routing tests, driven through both halves: the viewer answers each
//! event from the frame it painted and the session executes what came back,
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

use koshi_client::mouse::{MouseAction, TABLINE_DRAG_STEP};
use koshi_client::Client as ViewerClient;
use koshi_config::layer::{PartialKoshiConfig, PartialMouseConfig};
use koshi_config::types::WheelScroll;
use koshi_core::command::{
    FocusTabArgs, GridPos, NewPaneArgs, NewTabArgs, Selection, SelectionKind, TabTarget,
};
use koshi_core::geometry::{Direction, PaneArea, Point, Size};
use koshi_core::ids::SessionId;
use koshi_core::key::ModFlags;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind, ScrollDirection};
use koshi_layout::mode::LayoutMode;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_pty::error::PtyError;
use koshi_renderer::snapshot::{Delivery, MouseFrame, ViewerChrome};
use koshi_renderer::{hit_test, pane_local_cell, HitRegion};
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage};
use crate::runtime::bus::EventFilter;

fn runtime() -> (Server, ClientId) {
    let (runtime, _fake, client) = runtime_with_fake();
    (runtime, client)
}

/// A `new-pane` request with nothing chosen: the focused pane splits rightward.
fn new_pane_args() -> NewPaneArgs {
    NewPaneArgs {
        source: None,
        tab: None,
        direction: Direction::Right,
        stacked: false,
        cwd: None,
        command: None,
        client: None,
    }
}

/// The viewer half for `client_id`, on the stock settings: it holds the `mouse`
/// and `copy` config and answers every mouse event below before the session
/// hears about it.
fn viewer_for(runtime: &mut Server, client_id: ClientId) -> ViewerClient {
    ViewerClient::new(
        client_id,
        Size { cols: 80, rows: 24 },
        runtime.subscribe(client_id, EventFilter::All),
        TerminalCleanupGuard::new(),
    )
}

/// The same viewer, with its `mouse.wheel` setting on `wheel`.
fn viewer_with_wheel(
    runtime: &mut Server,
    client_id: ClientId,
    wheel: WheelScroll,
) -> ViewerClient {
    let mut viewer = viewer_for(runtime, client_id);
    viewer.load_startup_config(
        Some(PartialKoshiConfig {
            mouse: Some(PartialMouseConfig {
                wheel: Some(wheel),
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
/// what it means against the frame it is looking at, and only what it decided
/// reaches the session.
///
/// Timed far enough from any other that no two presses read as a double click —
/// the runtime tells a double click from two separate clicks by the gap between
/// them, so a test that pressed twice at the wall clock would double-click by
/// accident. A test that wants a real double click drives [`mouse_at`] with its
/// own instants.
fn mouse(runtime: &mut Server, viewer: &mut ViewerClient, input: MouseInput) {
    mouse_at(runtime, viewer, input, far_apart());
}

/// [`mouse`] with the instant the event happened at, for the tests that drive
/// the click threshold themselves.
fn mouse_at(runtime: &mut Server, viewer: &mut ViewerClient, input: MouseInput, now: Instant) {
    viewer.apply_events();
    let frame = MouseFrame::from(runtime.build_snapshot(viewer.id()).expect("snapshot"));
    let actions = viewer.handle_mouse(input, &frame, now);
    apply(runtime, viewer, &frame, actions);
}

/// Run everything the viewer decided, the way the binary's loop does.
fn apply(
    runtime: &mut Server,
    viewer: &mut ViewerClient,
    frame: &MouseFrame,
    actions: Vec<MouseAction>,
) {
    let client_id = viewer.id();
    let mut queue: VecDeque<MouseAction> = actions.into();
    while let Some(action) = queue.pop_front() {
        match action {
            MouseAction::Scroll { pane, up, lines } => {
                let top = runtime.scroll_pane_view(client_id, pane, up, lines);
                queue.extend(viewer.note_scroll_applied(pane, top, frame));
            }
            MouseAction::Forward { pane, mouse } => {
                let written = runtime.forward_mouse_to_pane(client_id, pane, mouse);
                if let (true, MouseKind::Press(button)) = (written, mouse.kind) {
                    viewer.note_press_forwarded(pane, button);
                }
            }
            MouseAction::AltScrollArrows { pane, up, count } => {
                runtime.write_alt_scroll_arrows(pane, up, count);
            }
            MouseAction::Resize {
                pane,
                side,
                step,
                count,
            } => {
                let applied = runtime.drag_resize(client_id, pane, side, step, count);
                viewer.note_resize_applied(pane, side, step, applied);
            }
            MouseAction::Command(command) => {
                let envelope = CommandEnvelope::new(
                    CommandId::new(),
                    CommandSource::mouse(client_id),
                    SystemTime::now(),
                    command,
                );
                let _ = runtime.submit_command(envelope);
            }
        }
    }
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
    runtime_sized(Size { cols: 80, rows: 24 })
}

/// [`runtime_with_fake`] on a viewport of `viewport`, for a case that needs
/// room for more panes than the stock 80 by 24 holds.
fn runtime_sized(viewport: Size) -> (Server, Arc<FakePtyBackend>, ClientId) {
    let fake = Arc::new(FakePtyBackend::new());
    let (tx, rx) = mpsc::channel();
    let mut runtime = Server::new(
        fake.clone(),
        Arc::new(NullSnapshotProvider),
        Arc::new(NullStorage),
        rx,
        tx,
    );
    let client = runtime
        .bootstrap_local(SessionId::new(), viewport, SystemTime::UNIX_EPOCH)
        .expect("bootstrap");
    (runtime, fake, client)
}

/// The active tab's panes top to bottom, each with the height the layout
/// solved for it.
fn stacked_panes(runtime: &Server, client: ClientId) -> Vec<(PaneId, u16)> {
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    let mut stacked: Vec<(u16, PaneId, u16)> = snapshot
        .session
        .active_tab
        .layout_solved
        .iter()
        .map(|slot| (slot.rect.origin.y, slot.pane_id, slot.rect.size.rows))
        .collect();
    stacked.sort_unstable();
    stacked
        .into_iter()
        .map(|(_, pane, rows)| (pane, rows))
        .collect()
}

/// Just the heights out of [`stacked_panes`].
fn heights(stacked: &[(PaneId, u16)]) -> Vec<u16> {
    stacked.iter().map(|&(_, rows)| rows).collect()
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
            if hit_test(snapshot.layout(ViewerChrome::default()), at)
                == (HitRegion::PaneContent { pane_id: pane })
            {
                let (col, row) =
                    pane_local_cell(snapshot.layout(ViewerChrome::default()), pane, at)
                        .expect("local cell");
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
    viewer: &ViewerClient,
    min_x: u16,
    pred: impl Fn(HitRegion) -> bool,
) -> u16 {
    let snapshot = runtime.build_snapshot(viewer.id()).expect("snapshot");
    let chrome = viewer.chrome(snapshot.client.active_tab);
    (min_x..snapshot.client.viewport.cols)
        .find(|&x| pred(hit_test(snapshot.layout(chrome), Point { x, y: 0 })))
        .expect("a matching tabline cell")
}

/// Where the viewer's tab strip is scrolled to, for the tab it is showing.
fn offset(runtime: &Server, viewer: &ViewerClient) -> Option<usize> {
    let snapshot = runtime.build_snapshot(viewer.id()).expect("snapshot");
    viewer.chrome(snapshot.client.active_tab).tabline_offset
}

/// Scroll the viewer's tab strip to index `to` by wheeling over the strip, the
/// only way a viewer's peek moves.
fn peek_to(runtime: &mut Server, viewer: &mut ViewerClient, to: usize) {
    // Each tick steps one tab; walking down from the far end lands on any index
    // whatever the strip was showing.
    let tabs = runtime
        .build_snapshot(viewer.id())
        .expect("snapshot")
        .session
        .tabs_metadata
        .len();
    for _ in 0..tabs {
        mouse(
            runtime,
            viewer,
            wheel(ScrollDirection::Up, Point { x: 0, y: 0 }),
        );
    }
    for _ in 0..to {
        mouse(
            runtime,
            viewer,
            wheel(ScrollDirection::Down, Point { x: 0, y: 0 }),
        );
    }
    assert_eq!(offset(runtime, viewer), Some(to), "the peek was set up");
}

#[test]
fn clicking_an_inactive_tab_focuses_it_and_clears_the_peek() {
    let (mut runtime, client) = runtime();
    for _ in 0..30 {
        add_tab(&mut runtime, client); // overflow the 80-column strip
    }
    let mut viewer = viewer_for(&mut runtime, client);

    // Peek from tab 0 so it is on the strip regardless of how wide the
    // auto-generated session and tab names happen to render, then click that
    // (now inactive) tab.
    peek_to(&mut runtime, &mut viewer, 0);
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    let first_tab = snapshot
        .session
        .tabs_metadata
        .iter()
        .find(|meta| meta.index == 0)
        .expect("a first tab")
        .id;
    let x = find_on_tabline(&runtime, &viewer, 0, |region| {
        region == HitRegion::Tab { tab_id: first_tab }
    });

    mouse(&mut runtime, &mut viewer, press(x, 0));

    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    assert_eq!(
        snapshot.client.active_tab, first_tab,
        "clicked tab is active"
    );
    assert_eq!(offset(&runtime, &viewer), None, "peek cleared on switch");
}

#[test]
fn a_tab_switch_by_any_route_reveals_the_new_tab() {
    // The peek belongs to the tab it was made on, so a switch driven from
    // anywhere — here a `focus-tab` command, not a click — reveals the new tab.
    let (mut runtime, client) = runtime();
    for _ in 0..30 {
        add_tab(&mut runtime, client);
    }
    let mut viewer = viewer_for(&mut runtime, client);
    peek_to(&mut runtime, &mut viewer, 3);

    let first_tab = runtime
        .build_snapshot(client)
        .expect("snapshot")
        .session
        .tabs_metadata
        .iter()
        .find(|meta| meta.index == 0)
        .expect("a first tab")
        .id;
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client),
        SystemTime::now(),
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Id(first_tab),
            client: Some(client),
        }),
    );
    let _ = runtime.dispatch(envelope);

    assert_eq!(offset(&runtime, &viewer), None, "the peek did not survive");
}

#[test]
fn clicking_the_right_scroll_arrow_peeks_toward_the_end() {
    let (mut runtime, client) = runtime();
    for _ in 0..30 {
        add_tab(&mut runtime, client); // overflow the 80-column strip
    }
    let mut viewer = viewer_for(&mut runtime, client);
    peek_to(&mut runtime, &mut viewer, 0);

    let x = find_on_tabline(&runtime, &viewer, 0, |region| {
        matches!(region, HitRegion::TablineScrollRight { .. })
    });
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    let chrome = viewer.chrome(snapshot.client.active_tab);
    let to = match hit_test(snapshot.layout(chrome), Point { x, y: 0 }) {
        HitRegion::TablineScrollRight { to } => to,
        other => panic!("expected a right scroll arrow, got {other:?}"),
    };

    mouse(&mut runtime, &mut viewer, press(x, 0));

    assert!(to > 0, "the right arrow scrolls toward the end");
    assert_eq!(offset(&runtime, &viewer), Some(to));
}

#[test]
fn wheel_over_the_tabline_steps_the_offset() {
    let (mut runtime, client) = runtime();
    for _ in 0..30 {
        add_tab(&mut runtime, client);
    }
    let mut viewer = viewer_for(&mut runtime, client);
    peek_to(&mut runtime, &mut viewer, 0);

    let x = find_on_tabline(&runtime, &viewer, 0, |region| {
        matches!(
            region,
            HitRegion::Tab { .. } | HitRegion::TablineScrollRight { .. }
        )
    });

    mouse(
        &mut runtime,
        &mut viewer,
        wheel(ScrollDirection::Down, Point { x, y: 0 }),
    );

    assert_eq!(
        offset(&runtime, &viewer),
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
    let mut viewer = viewer_for(&mut runtime, client);
    peek_to(&mut runtime, &mut viewer, 2);

    // Row 10 is pane content, not the tabline.
    mouse(
        &mut runtime,
        &mut viewer,
        wheel(ScrollDirection::Down, Point { x: 40, y: 10 }),
    );

    assert_eq!(
        offset(&runtime, &viewer),
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
    let mut viewer = viewer_for(&mut runtime, client);
    peek_to(&mut runtime, &mut viewer, 2);

    // Buttonless motion over the tabline scrolls nothing and begins no drag.
    mouse(
        &mut runtime,
        &mut viewer,
        MouseInput {
            kind: MouseKind::Motion,
            at: Point { x: 5, y: 0 },
            mods: ModFlags::NONE,
        },
    );
    // A right press over a tab is neither a focus nor a scroll.
    mouse(
        &mut runtime,
        &mut viewer,
        MouseInput {
            kind: MouseKind::Press(MouseButton::Right),
            at: Point { x: 5, y: 0 },
            mods: ModFlags::NONE,
        },
    );

    assert_eq!(
        offset(&runtime, &viewer),
        Some(2),
        "ignored events do not scroll"
    );

    // No drag began: a left drag now scrolls nothing either.
    mouse(
        &mut runtime,
        &mut viewer,
        drag(5 + TABLINE_DRAG_STEP as u16, 0),
    );
    assert_eq!(
        offset(&runtime, &viewer),
        Some(2),
        "ignored events begin no drag"
    );
}

#[test]
fn dragging_scrolls_from_the_anchor_and_release_ends_it() {
    let (mut runtime, client) = runtime();
    for _ in 0..30 {
        add_tab(&mut runtime, client);
    }
    let mut viewer = viewer_for(&mut runtime, client);
    peek_to(&mut runtime, &mut viewer, 2);

    // Press a bare tabline cell far enough along the row that a two-step drag
    // to its left stays on screen.
    let anchor_x = find_on_tabline(&runtime, &viewer, 2 * TABLINE_DRAG_STEP as u16, |region| {
        region == HitRegion::Tabline
    });
    mouse(&mut runtime, &mut viewer, press(anchor_x, 0));

    // Drag left by two steps' worth of cells: scroll two tabs toward the end.
    let x = anchor_x - 2 * TABLINE_DRAG_STEP as u16;
    mouse(&mut runtime, &mut viewer, drag(x, 0));
    assert_eq!(
        offset(&runtime, &viewer),
        Some(4),
        "two steps past anchor 2"
    );

    // Release ends the drag, leaving the scrolled offset.
    mouse(&mut runtime, &mut viewer, release());
    assert_eq!(
        offset(&runtime, &viewer),
        Some(4),
        "offset stays after release"
    );

    // And a later drag with no press behind it scrolls nothing.
    mouse(
        &mut runtime,
        &mut viewer,
        drag(anchor_x + 2 * TABLINE_DRAG_STEP as u16, 0),
    );
    assert_eq!(offset(&runtime, &viewer), Some(4), "release ended the drag");
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
        Command::NewPane(new_pane_args()),
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
        if let HitRegion::PaneBorder { pane_id, side } =
            hit_test(snapshot.layout(ViewerChrome::default()), Point { x, y })
        {
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
fn dragging_a_vertical_border_resizes_the_grabbed_pane_live() {
    let (mut runtime, client) = runtime();
    let mut viewer = viewer_for(&mut runtime, client);
    split_focused(&mut runtime, client);

    let (cell, pane, side) = find_vertical_border(&runtime, client);
    let before = pane_cols(&runtime, client, pane);

    mouse(&mut runtime, &mut viewer, press(cell.x, cell.y));
    mouse(
        &mut runtime,
        &mut viewer,
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
    let mut viewer = viewer_for(&mut runtime, client);
    split_focused(&mut runtime, client);

    let (cell, pane, side) = find_vertical_border(&runtime, client);
    let before = pane_cols(&runtime, client, pane);

    mouse(&mut runtime, &mut viewer, press(cell.x, cell.y));

    // Drag three cells inward to shrink the pane.
    mouse(
        &mut runtime,
        &mut viewer,
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
        &mut viewer,
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
    let mut viewer = viewer_for(&mut runtime, client);
    split_focused(&mut runtime, client);

    let (cell, pane, side) = find_vertical_border(&runtime, client);
    mouse(&mut runtime, &mut viewer, press(cell.x, cell.y));
    mouse(
        &mut runtime,
        &mut viewer,
        drag(outward_x(side, cell.x, 2), cell.y),
    );
    let after_drag = pane_cols(&runtime, client, pane);

    mouse(&mut runtime, &mut viewer, release());

    // With no resize drag in progress, a stray drag resizes nothing.
    mouse(
        &mut runtime,
        &mut viewer,
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
    let mut viewer = viewer_for(&mut runtime, client);
    split_focused(&mut runtime, client);

    let (cell, pane, side) = find_vertical_border(&runtime, client);
    let before = pane_cols(&runtime, client, pane);
    let viewport_cols = runtime.build_snapshot(client).unwrap().client.viewport.cols;

    mouse(&mut runtime, &mut viewer, press(cell.x, cell.y));

    // One big jump past the wall: the drag is applied a cell at a time, so it
    // grows the pane as far as the neighbor can donate instead of refusing the
    // whole move.
    mouse(
        &mut runtime,
        &mut viewer,
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
        &mut viewer,
        drag(outward_edge_x(side, viewport_cols), cell.y),
    );
    assert_eq!(
        pane_cols(&runtime, client, pane),
        grown,
        "held at the wall while the pointer overshoots"
    );

    // Reverse straight back to the original border cell: the anchor held at the
    // wall, so the pane shrinks back with no dead zone.
    mouse(&mut runtime, &mut viewer, drag(cell.x, cell.y));
    assert_eq!(
        pane_cols(&runtime, client, pane),
        before,
        "a reverse drag returns the border to where it started, no lag"
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
            direction: Direction::Down,
            ..new_pane_args()
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
        if let HitRegion::PaneBorder { pane_id, side } =
            hit_test(snapshot.layout(ViewerChrome::default()), Point { x, y })
        {
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
        if let HitRegion::PaneBorder { pane_id, side } =
            hit_test(snapshot.layout(ViewerChrome::default()), Point { x, y })
        {
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
    let mut viewer = viewer_for(&mut runtime, client);
    split_focused_vertical(&mut runtime, client);

    let (cell, pane, side) = find_horizontal_border(&runtime, client);
    let before = pane_rows(&runtime, client, pane);

    mouse(&mut runtime, &mut viewer, press(cell.x, cell.y));
    mouse(
        &mut runtime,
        &mut viewer,
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
    let mut viewer = viewer_for(&mut runtime, client);
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
    mouse(&mut runtime, &mut viewer, press(cell.x, cell.y));

    // A drag inward after that changes nothing either.
    mouse(&mut runtime, &mut viewer, drag(cell.x - 3, cell.y));
    assert_eq!(
        pane_cols(&runtime, client, pane),
        before,
        "grabbing the terminal's outer edge resizes nothing"
    );
}

#[test]
fn grabbing_the_frame_of_a_fullscreen_pane_starts_no_resize() {
    let (mut runtime, client) = runtime();
    let mut viewer = viewer_for(&mut runtime, client);
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
    mouse(&mut runtime, &mut viewer, press(cell.x, cell.y));

    mouse(&mut runtime, &mut viewer, drag(cell.x - 3, cell.y));
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
    let mut viewer = viewer_for(&mut runtime, client);
    let pane = only_pane(&runtime);
    // The program turns on normal tracking with SGR encoding.
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    let (at, col, row) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, &mut viewer, press(at.x, at.y));

    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![format!("\x1b[<0;{col};{row}M").into_bytes()],
        "the click in the focused pane is forwarded as an SGR report"
    );
}

#[test]
fn a_click_forwards_nothing_when_the_program_wants_no_mouse() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let mut viewer = viewer_for(&mut runtime, client);
    let pane = only_pane(&runtime);
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, &mut viewer, press(at.x, at.y));

    assert_eq!(
        fake.writes(pane).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "a pane in no mouse mode receives nothing"
    );
}

#[test]
fn a_press_drag_release_gesture_forwards_each_event() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let mut viewer = viewer_for(&mut runtime, client);
    let pane = only_pane(&runtime);
    // Button-event tracking reports drags; SGR encoding.
    runtime.handle_pty_output(pane, b"\x1b[?1002h\x1b[?1006h");
    let (at, col, row) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, &mut viewer, press(at.x, at.y));
    mouse(&mut runtime, &mut viewer, drag(at.x, at.y));
    mouse(
        &mut runtime,
        &mut viewer,
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
fn a_drag_reports_the_cell_it_moved_to_with_the_column_and_row_the_right_way_round() {
    // Every other forwarding test presses `a_content_cell`, which is the pane's
    // top-left content cell — column 1, row 1. Two equal numbers cannot show
    // which is which, so those tests pass just as well with the pair swapped.
    // This one moves three columns across and one row down, where a swap reads
    // `4;2` as `2;4`.
    let (mut runtime, fake, client) = runtime_with_fake();
    let mut viewer = viewer_for(&mut runtime, client);
    let pane = only_pane(&runtime);
    // Button-event tracking reports drags; SGR encoding.
    runtime.handle_pty_output(pane, b"\x1b[?1002h\x1b[?1006h");
    let (start, col, row) = a_content_cell(&runtime, client, pane);

    mouse(&mut runtime, &mut viewer, press(start.x, start.y));
    mouse(&mut runtime, &mut viewer, drag(start.x + 3, start.y + 1));

    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![
            format!("\x1b[<0;{col};{row}M").into_bytes(),
            format!("\x1b[<32;{};{}M", col + 3, row + 1).into_bytes(),
        ],
        "the drag reports the cell it moved to, column first"
    );
}

#[test]
fn a_mouse_select_gesture_over_a_mouse_aware_program_forwards_nothing() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let mut viewer = viewer_for(&mut runtime, client);
    let pane = only_pane(&runtime);
    // Button-event tracking would report a bare drag; SGR encoding.
    runtime.handle_pty_output(pane, b"\x1b[?1002h\x1b[?1006h");
    // Grab the mouse for koshi selection, the way the binding does; the viewer
    // takes the change off its subscription before the next event.
    let _ = runtime.submit_command(CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client),
        SystemTime::now(),
        Command::ToggleMouseSelect,
    ));
    let (at, _, _) = a_content_cell(&runtime, client, pane);
    let gesture = |kind| MouseInput {
        kind,
        at,
        mods: ModFlags::NONE,
    };

    mouse(
        &mut runtime,
        &mut viewer,
        gesture(MouseKind::Press(MouseButton::Left)),
    );
    mouse(
        &mut runtime,
        &mut viewer,
        gesture(MouseKind::Drag(MouseButton::Left)),
    );
    mouse(
        &mut runtime,
        &mut viewer,
        gesture(MouseKind::Release(MouseButton::Left)),
    );

    assert_eq!(
        fake.writes(pane).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "in mouse-select mode the gesture is koshi's selection; the program is sent nothing"
    );
}

#[test]
fn the_forward_door_reports_whether_the_pane_was_written_to() {
    // The report the door gives back is what the viewer captures a gesture on,
    // so it must be false for exactly the events the pane never saw.
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    let (at, col, row) = a_content_cell(&runtime, client, pane);

    // The program asked for no mouse: nothing is written, and the door says so.
    assert!(!runtime.forward_mouse_to_pane(client, pane, press(at.x, at.y)));
    assert_eq!(
        fake.writes(pane).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "a pane in no mouse mode receives nothing"
    );

    // Normal tracking with SGR encoding: the press is written, and reported.
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    assert!(runtime.forward_mouse_to_pane(client, pane, press(at.x, at.y)));
    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![format!("\x1b[<0;{col};{row}M").into_bytes()],
        "the press reached the program"
    );

    // The pane refuses the bytes: the door says so, and no gesture is captured
    // on the strength of a press that never landed.
    fake.fail_writes_on(pane, PtyError::UnknownPane { pane });
    assert!(!runtime.forward_mouse_to_pane(client, pane, press(at.x, at.y)));
    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![format!("\x1b[<0;{col};{row}M").into_bytes()],
        "the refused press left no record"
    );
}

/// A tab whose only viewer reports [`PaneArea::Starving`] has no effective
/// size: every pane is suppressed and the click is forwarded to no pane.
#[test]
fn a_click_from_a_starving_sole_viewer_forwards_nothing() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    // Read the cell off the layout while the client still sizes the tab.
    let (at, _col, _row) = a_content_cell(&runtime, client, pane);
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");

    runtime
        .session_for_client_mut(client)
        .expect("session")
        .clients
        .get_mut(client)
        .expect("client")
        .update_pane_area(Some(PaneArea::Starving));

    assert!(!runtime.forward_mouse_to_pane(client, pane, press(at.x, at.y)));
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn a_bare_move_forwards_only_in_any_motion_mode() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let mut viewer = viewer_for(&mut runtime, client);
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
    mouse(&mut runtime, &mut viewer, motion);
    assert_eq!(
        fake.writes(pane).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "normal tracking ignores a bare move"
    );

    // Any-motion tracking reports it: no-button 3 + motion bit 32 = 35.
    runtime.handle_pty_output(pane, b"\x1b[?1003h");
    mouse(&mut runtime, &mut viewer, motion);
    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![format!("\x1b[<35;{col};{row}M").into_bytes()],
        "any-motion tracking reports the move"
    );
}

#[test]
fn a_captured_release_is_re_stamped_to_the_pressed_button() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let mut viewer = viewer_for(&mut runtime, client);
    let pane = only_pane(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    let (at, col, row) = a_content_cell(&runtime, client, pane);

    // A right press captures the gesture (button 2).
    mouse(
        &mut runtime,
        &mut viewer,
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
        &mut viewer,
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
    let mut viewer = viewer_for(&mut runtime, client);
    let pane = only_pane(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1002h\x1b[?1006h");
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    // A drag arrives without a press to capture the gesture (a release with no
    // matching press is the orphan-release case) — nothing is forwarded.
    mouse(&mut runtime, &mut viewer, drag(at.x, at.y));
    mouse(
        &mut runtime,
        &mut viewer,
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
    let mut viewer = viewer_for(&mut runtime, client);
    let pane = only_pane(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1002h\x1b[?1006h");
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    // Press inside the pane to capture the gesture, then drag far past its top-
    // left corner (0, 0 is the tabline row, outside the pane); the captured drag
    // clamps to the pane's first cell.
    mouse(&mut runtime, &mut viewer, press(at.x, at.y));
    mouse(&mut runtime, &mut viewer, drag(0, 0));

    assert_eq!(
        fake.writes(pane).expect("writes").last().expect("a drag"),
        &b"\x1b[<32;1;1M".to_vec(),
        "the drag clamps to the pane's top-left cell (1, 1)"
    );
}

#[test]
fn border_resize_off_leaves_a_border_press_inert() {
    let (mut runtime, client) = runtime();
    // The setting is the viewer's own, so it is the viewer that must be built
    // on it.
    let mut viewer = viewer_for(&mut runtime, client);
    viewer.load_startup_config(
        Some(PartialKoshiConfig {
            mouse: Some(PartialMouseConfig {
                border_resize: Some(false),
                ..PartialMouseConfig::default()
            }),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );
    split_focused(&mut runtime, client);

    let (cell, pane, side) = find_vertical_border(&runtime, client);
    let before = pane_cols(&runtime, client, pane);

    mouse(&mut runtime, &mut viewer, press(cell.x, cell.y));
    mouse(
        &mut runtime,
        &mut viewer,
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
    let mut viewer = viewer_for(&mut runtime, client);
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

    mouse(&mut runtime, &mut viewer, press(at.x, at.y));

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

/// The pane the viewer's pointer is marked as hovering over.
fn hovered(runtime: &Server, viewer: &ViewerClient) -> Option<PaneId> {
    let snapshot = runtime.build_snapshot(viewer.id()).expect("snapshot");
    viewer.chrome(snapshot.client.active_tab).hovered_pane
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
    let mut viewer = viewer_for(&mut runtime, client);

    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Up, at));

    // scroll_lines defaults to 3, so one wheel up moves the view three lines.
    assert_eq!(scroll_offset(&runtime, client, pane), 3, "wheel up scrolls");
    assert_eq!(
        offset(&runtime, &viewer),
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

    let mut viewer = viewer_for(&mut runtime, client);
    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Up, at));
    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Up, at));
    assert_eq!(
        scroll_offset(&runtime, client, pane),
        6,
        "two ups, six lines"
    );

    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Down, at));
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
    let mut viewer = viewer_for(&mut runtime, client);

    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Up, at));

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
    let mut viewer = viewer_for(&mut runtime, client);

    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Up, at));

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
    let mut viewer = viewer_for(&mut runtime, client);

    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Up, at));
    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![b"\x1b[A\x1b[A\x1b[A".to_vec()],
        "wheel up becomes three up-arrows under default cursor keys"
    );

    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Down, at));
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
    let mut viewer = viewer_for(&mut runtime, client);

    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Up, at));

    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![b"\x1bOA\x1bOA\x1bOA".to_vec()],
        "application cursor keys send the SS3 form ESC O A"
    );
}

#[test]
fn the_ignore_wheel_config_does_nothing_over_a_plain_pane() {
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    feed_scrollback(&mut runtime, pane, 40);
    let (at, _, _) = a_content_cell(&runtime, client, pane);
    // The setting is the viewer's own, so it is the viewer that must be built
    // on it.
    let mut viewer = viewer_with_wheel(&mut runtime, client, WheelScroll::Ignore);

    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Up, at));

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

    let mut viewer = viewer_for(&mut runtime, client);

    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Left, at));

    assert_eq!(
        scroll_offset(&runtime, client, pane),
        0,
        "a horizontal wheel leaves the vertical scrollback view alone"
    );
}

#[test]
fn a_move_marks_the_hovered_pane_and_clears_it_off_a_pane() {
    let (mut runtime, client) = runtime();
    let mut viewer = viewer_for(&mut runtime, client);
    let pane = only_pane(&runtime);
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    mouse(
        &mut runtime,
        &mut viewer,
        MouseInput {
            kind: MouseKind::Motion,
            at,
            mods: ModFlags::NONE,
        },
    );
    assert_eq!(
        hovered(&runtime, &viewer),
        Some(pane),
        "a move over pane content marks it hovered"
    );

    // Row 0 is the tabline, not a pane.
    mouse(
        &mut runtime,
        &mut viewer,
        MouseInput {
            kind: MouseKind::Motion,
            at: Point { x: 0, y: 0 },
            mods: ModFlags::NONE,
        },
    );
    assert_eq!(
        hovered(&runtime, &viewer),
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
    let mut viewer = viewer_for(&mut runtime, client);

    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Up, at));

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
    let mut viewer = viewer_for(&mut runtime, client);
    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Up, cell));

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
    let mut viewer = viewer_for(&mut runtime, client);

    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Up, at));

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
fn a_highlight_holds_the_view_even_over_a_mouse_reporting_program() {
    // The frame carries whether this client has a highlight in the pane, and a
    // highlight outranks the program's mouse mode: koshi scrolls its own view
    // and the program is told nothing.
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    feed_scrollback(&mut runtime, pane, 40);
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    set_highlight(&mut runtime, client, pane);
    let (at, _, _) = a_content_cell(&runtime, client, pane);
    let mut viewer = viewer_for(&mut runtime, client);

    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Up, at));

    assert_eq!(
        scroll_offset(&runtime, client, pane),
        3,
        "the highlighted view scrolls"
    );
    assert_eq!(
        fake.writes(pane).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "the program receives no report"
    );
}

#[test]
fn a_forwarded_wheel_is_dropped_when_the_program_turned_the_mouse_off() {
    // The viewer decides from the frame it painted, so it can name a pane whose
    // program has since stopped asking for the mouse. The session re-reads the
    // live mode when it writes and drops the tick.
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    let (at, _, _) = a_content_cell(&runtime, client, pane);
    let mut viewer = viewer_for(&mut runtime, client);
    let tick = wheel(ScrollDirection::Up, at);
    let frame = MouseFrame::from(runtime.build_snapshot(client).expect("snapshot"));
    let decision = viewer
        .handle_mouse_wheel(tick, &frame)
        .expect("a wheel tick decides");
    assert_eq!(
        decision.action,
        Some(MouseAction::Forward { pane, mouse: tick }),
        "the painted frame still said the program wanted the mouse"
    );

    // The program turns mouse reporting off between that frame and the write.
    runtime.handle_pty_output(pane, b"\x1b[?1000l");
    runtime.forward_mouse_to_pane(client, pane, tick);

    assert_eq!(
        fake.writes(pane).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "nothing is written to a program that no longer wants the mouse"
    );
}

#[test]
fn alt_scroll_arrows_follow_the_cursor_key_mode_at_the_moment_they_are_written() {
    // DECCKM (`?1`) is read when the arrows are written, not when the frame the
    // viewer decided from was painted.
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1049h\x1b[?1007h");
    let (at, _, _) = a_content_cell(&runtime, client, pane);
    let mut viewer = viewer_for(&mut runtime, client);
    let frame = MouseFrame::from(runtime.build_snapshot(client).expect("snapshot"));
    let decision = viewer
        .handle_mouse_wheel(wheel(ScrollDirection::Up, at), &frame)
        .expect("a wheel tick decides");
    assert_eq!(
        decision.action,
        Some(MouseAction::AltScrollArrows {
            pane,
            up: true,
            count: 3,
        })
    );

    // The program switches to application cursor keys before the write.
    runtime.handle_pty_output(pane, b"\x1b[?1h");
    runtime.write_alt_scroll_arrows(pane, true, 3);

    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![b"\x1bOA\x1bOA\x1bOA".to_vec()],
        "the SS3 form the live mode asks for, not the frame's"
    );
}

#[test]
fn arrow_keys_are_dropped_when_the_pane_left_the_alternate_screen_before_the_write() {
    // The viewer decides from the frame it painted, so it can name a pane whose
    // program has since left the alternate screen. Writing the arrows then would
    // put them in the shell prompt underneath and recall its history.
    let (mut runtime, fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1049h\x1b[?1007h");
    let (at, _, _) = a_content_cell(&runtime, client, pane);
    let mut viewer = viewer_for(&mut runtime, client);
    let frame = MouseFrame::from(runtime.build_snapshot(client).expect("snapshot"));
    let decision = viewer
        .handle_mouse_wheel(wheel(ScrollDirection::Up, at), &frame)
        .expect("a wheel tick decides");
    assert_eq!(
        decision.action,
        Some(MouseAction::AltScrollArrows {
            pane,
            up: true,
            count: 3,
        }),
        "the painted frame still said the pane was on the alternate screen"
    );

    // The program leaves the alternate screen before the write.
    runtime.handle_pty_output(pane, b"\x1b[?1049l");
    runtime.write_alt_scroll_arrows(pane, true, 3);

    assert_eq!(
        fake.writes(pane).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "nothing is written to a pane that is back on the primary screen"
    );
}

#[test]
fn the_write_doors_do_nothing_for_a_pane_that_is_gone() {
    // The viewer names a pane off a frame it painted, so it can name one the
    // session has since released. Every door must answer that with nothing.
    let (mut runtime, fake, client) = runtime_with_fake();
    let live = only_pane(&runtime);
    let gone = PaneId::new();

    runtime.scroll_pane_view(client, gone, true, 3);
    let forwarded = runtime.forward_mouse_to_pane(
        client,
        gone,
        wheel(ScrollDirection::Up, Point { x: 5, y: 5 }),
    );
    runtime.write_alt_scroll_arrows(gone, true, 3);
    let applied = runtime.drag_resize(client, gone, Direction::Right, 1, 3);

    assert!(
        fake.writes(gone).is_err(),
        "a gone pane was never opened, so it has no write log at all"
    );
    assert_eq!(
        fake.writes(live).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "and nothing landed on the live pane instead"
    );
    assert_eq!(
        scroll_offset(&runtime, client, gone),
        0,
        "no view was stored for a gone pane"
    );
    assert_eq!(applied, 0, "no border of a gone pane moved");
    assert!(!forwarded, "no report was written to a gone pane");
}

#[test]
fn a_zero_line_notch_sends_no_arrow_keys() {
    // `mouse.scroll_lines 0` reaches the door as a count of zero.
    let (mut runtime, fake, _client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1049h\x1b[?1007h");

    runtime.write_alt_scroll_arrows(pane, true, 0);

    assert_eq!(
        fake.writes(pane).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "a zero-line notch sends no arrows at all"
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
    let mut viewer = viewer_for(&mut runtime, client);

    mouse(&mut runtime, &mut viewer, wheel(ScrollDirection::Up, at));

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
                hit_test(snapshot.layout(ViewerChrome::default()), at),
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
    let mut viewer = viewer_for(&mut runtime, client);
    mouse(
        &mut runtime,
        &mut viewer,
        wheel(ScrollDirection::Up, chrome),
    );

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

#[test]
fn a_forward_decided_from_a_stale_frame_writes_nothing_once_tracking_is_off() {
    // The viewer answers from the frame it last painted, which can be one event
    // out of date. Here the program turns mouse reporting off after that frame
    // was painted and before the press is applied. The session reads the live
    // level at the moment of the write, so the program that stopped asking gets
    // nothing.
    let (mut runtime, fake, client) = runtime_with_fake();
    let mut viewer = viewer_for(&mut runtime, client);
    let pane = only_pane(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    let (at, _, _) = a_content_cell(&runtime, client, pane);

    let frame = MouseFrame::from(runtime.build_snapshot(client).expect("snapshot"));
    let actions = viewer.handle_mouse(press(at.x, at.y), &frame, far_apart());
    assert_eq!(
        actions,
        vec![MouseAction::Forward {
            pane,
            mouse: press(at.x, at.y),
        }],
        "the frame said the program wanted the mouse"
    );

    // The program turns reporting off; no frame is painted in between.
    runtime.handle_pty_output(pane, b"\x1b[?1000l");
    apply(&mut runtime, &mut viewer, &frame, actions);

    assert_eq!(
        fake.writes(pane).expect("writes"),
        Vec::<Vec<u8>>::new(),
        "the live level, not the painted one, decides what is written"
    );
}

/// Stack a second pane onto the focused one, so the tab holds one stack whose
/// members share a rect: the active member shows its content and the other
/// collapses to a one-row header strip.
fn stack_onto_focused(runtime: &mut Server, client: ClientId) {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client),
        SystemTime::now(),
        Command::NewPane(NewPaneArgs {
            stacked: true,
            ..new_pane_args()
        }),
    );
    let _ = runtime.dispatch(envelope);
}

/// A border cell of a drawn pane that sits right against a collapsed stack
/// member's header strip, with the pane and the side it is on. Panics if the
/// frame has no such cell.
fn find_border_against_a_header(runtime: &Server, client: ClientId) -> (Point, PaneId) {
    let snapshot = runtime.build_snapshot(client).expect("snapshot");
    let viewport = snapshot.client.viewport;
    let region = |at: Point| hit_test(snapshot.layout(ViewerChrome::default()), at);
    let x = viewport.cols / 2;
    for y in 1..viewport.rows - 1 {
        let HitRegion::PaneBorder { pane_id, side } = region(Point { x, y }) else {
            continue;
        };
        let touching = match side {
            Direction::Up if y > 0 => region(Point { x, y: y - 1 }),
            Direction::Down => region(Point { x, y: y + 1 }),
            _ => continue,
        };
        if matches!(touching, HitRegion::StackHeader { .. }) {
            return (Point { x, y }, pane_id);
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
    let (mut runtime, client) = runtime();
    let mut viewer = viewer_for(&mut runtime, client);
    stack_onto_focused(&mut runtime, client);

    let (cell, _) = find_border_against_a_header(&runtime, client);
    let frame = MouseFrame::from(runtime.build_snapshot(client).expect("snapshot"));

    let pressed = viewer.handle_mouse(press(cell.x, cell.y), &frame, far_apart());
    assert_eq!(
        pressed,
        Vec::new(),
        "the header strip is no neighbor to resize against, so the press \
         begins no drag"
    );

    let dragged = viewer.handle_mouse(drag(cell.x, cell.y + 3), &frame, far_apart());
    assert_eq!(
        dragged,
        Vec::new(),
        "with no drag under way the pointer asks for no border move"
    );
}

/// The runtime, its fake PTY backend, a client, and that client's own
/// subscriber queue — the queue a mouse round's answer lands on.
fn runtime_with_queue() -> (
    Server,
    Arc<FakePtyBackend>,
    ClientId,
    mpsc::Receiver<Delivery>,
) {
    let (mut runtime, fake, client) = runtime_with_fake();
    let queue = runtime.subscribe(client, EventFilter::All);
    (runtime, fake, client, queue)
}

/// Every mouse answer waiting on `queue`, in order, as its round id and its
/// list. The frames and events that share the queue are left out.
fn answers(queue: &mpsc::Receiver<Delivery>) -> Vec<(u64, Vec<MouseAnswer>)> {
    queue
        .try_iter()
        .filter_map(|delivery| match delivery {
            Delivery::MouseAnswer {
                request_id,
                answers,
            } => Some((request_id, answers)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_round_holding_one_scroll_answers_with_the_line_the_view_landed_on() {
    let (mut runtime, _fake, client, queue) = runtime_with_queue();
    let pane = only_pane(&runtime);
    feed_scrollback(&mut runtime, pane, 40);

    runtime.run_client_mouse(
        client,
        7,
        vec![WireMouseAction::Scroll {
            pane,
            up: true,
            lines: 5,
        }],
    );

    assert_eq!(
        scroll_offset(&runtime, client, pane),
        5,
        "the view moved five lines into history"
    );
    // A zero-line move reads the same line back off the door without touching
    // the view, so the answer is checked against the door's own number.
    let top = runtime.scroll_pane_view(client, pane, true, 0);
    // 40 lines through a 20-row pane push 21 into history, so the live view's
    // top row is line 21 and five lines up is line 16.
    assert_eq!(top, Some(16));
    assert_eq!(
        answers(&queue),
        vec![(7, vec![MouseAnswer::Scrolled { pane, top }])],
        "the round is answered with the line the scroll landed on"
    );
}

#[test]
fn a_round_holding_one_border_move_answers_with_the_cells_it_took() {
    let (mut runtime, _fake, client, queue) = runtime_with_queue();
    let pane = only_pane(&runtime);
    split_focused(&mut runtime, client);
    let before = pane_cols(&runtime, client, pane);

    runtime.run_client_mouse(
        client,
        8,
        vec![WireMouseAction::Resize {
            pane,
            side: Direction::Right,
            step: 1,
            count: 3,
        }],
    );

    assert_eq!(
        pane_cols(&runtime, client, pane),
        before + 3,
        "the border moved three cells"
    );
    assert_eq!(
        answers(&queue),
        vec![(
            8,
            vec![MouseAnswer::Resized {
                pane,
                side: Direction::Right,
                step: 1,
                applied: 3,
            }]
        )],
        "the answer names the border it moved and the cells it really took"
    );
}

#[test]
fn a_round_holding_one_forward_writes_the_report_and_answers_with_an_empty_list() {
    let (mut runtime, fake, client, queue) = runtime_with_queue();
    let pane = only_pane(&runtime);
    // The program turns on normal tracking with SGR encoding.
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    let (at, col, row) = a_content_cell(&runtime, client, pane);

    runtime.run_client_mouse(
        client,
        9,
        vec![WireMouseAction::Forward {
            pane,
            mouse: press(at.x, at.y),
        }],
    );

    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![format!("\x1b[<0;{col};{row}M").into_bytes()],
        "the press reached the program as an SGR report"
    );
    assert_eq!(
        answers(&queue),
        vec![(9, Vec::new())],
        "a forward reports nothing, and the round is still answered"
    );
}

#[test]
fn a_round_holding_one_alt_scroll_writes_the_arrows_and_answers_with_an_empty_list() {
    let (mut runtime, fake, client, queue) = runtime_with_queue();
    let pane = only_pane(&runtime);
    // Enter the alternate screen and turn alternate-scroll on.
    runtime.handle_pty_output(pane, b"\x1b[?1049h\x1b[?1007h");

    runtime.run_client_mouse(
        client,
        10,
        vec![WireMouseAction::AltScrollArrows {
            pane,
            up: true,
            count: 3,
        }],
    );

    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![b"\x1b[A\x1b[A\x1b[A".to_vec()],
        "wheel up became three up-arrows under default cursor keys"
    );
    assert_eq!(
        answers(&queue),
        vec![(10, Vec::new())],
        "arrows report nothing, and the round is still answered"
    );
}

#[test]
fn a_round_holding_one_command_runs_it_and_answers_with_an_empty_list() {
    let (mut runtime, _fake, client, queue) = runtime_with_queue();
    let tabs = |runtime: &Server| {
        runtime
            .build_snapshot(client)
            .expect("snapshot")
            .session
            .tabs_metadata
            .len()
    };
    assert_eq!(tabs(&runtime), 1, "the session starts on one tab");

    runtime.run_client_mouse(
        client,
        11,
        vec![WireMouseAction::Command(Box::new(Command::NewTab(
            NewTabArgs::default(),
        )))],
    );

    assert_eq!(tabs(&runtime), 2, "the command the round carried ran");
    assert_eq!(
        answers(&queue),
        vec![(11, Vec::new())],
        "a command reports nothing, and the round is still answered"
    );
}

#[test]
fn a_round_that_reports_nothing_is_still_answered() {
    // The answer is what releases the viewer's gate: skipping it stalls that
    // client's whole mouse uplink until it detaches.
    let (mut runtime, fake, client, queue) = runtime_with_queue();
    let pane = only_pane(&runtime);
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    let (at, col, row) = a_content_cell(&runtime, client, pane);
    let forward = WireMouseAction::Forward {
        pane,
        mouse: press(at.x, at.y),
    };

    runtime.run_client_mouse(client, 12, vec![forward.clone(), forward]);

    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![
            format!("\x1b[<0;{col};{row}M").into_bytes(),
            format!("\x1b[<0;{col};{row}M").into_bytes(),
        ],
        "both presses reached the program"
    );
    assert_eq!(
        answers(&queue),
        vec![(12, Vec::new())],
        "exactly one answer, holding an empty list"
    );
}

#[test]
fn the_answers_follow_the_order_of_the_actions_that_reported() {
    let (mut runtime, fake, client, queue) = runtime_with_queue();
    let pane = only_pane(&runtime);
    split_focused(&mut runtime, client);
    feed_scrollback(&mut runtime, pane, 40);
    runtime.handle_pty_output(pane, b"\x1b[?1000h\x1b[?1006h");
    let (at, col, row) = a_content_cell(&runtime, client, pane);
    let before = pane_cols(&runtime, client, pane);

    runtime.run_client_mouse(
        client,
        13,
        vec![
            WireMouseAction::Scroll {
                pane,
                up: true,
                lines: 5,
            },
            WireMouseAction::Forward {
                pane,
                mouse: press(at.x, at.y),
            },
            WireMouseAction::Resize {
                pane,
                side: Direction::Right,
                step: 1,
                count: 3,
            },
        ],
    );

    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![format!("\x1b[<0;{col};{row}M").into_bytes()],
        "the forward in the middle of the round still reached the program"
    );
    assert_eq!(
        pane_cols(&runtime, client, pane),
        before + 3,
        "the border move at the end of the round still ran"
    );
    assert_eq!(
        answers(&queue),
        vec![(
            13,
            vec![
                MouseAnswer::Scrolled {
                    pane,
                    top: Some(16),
                },
                MouseAnswer::Resized {
                    pane,
                    side: Direction::Right,
                    step: 1,
                    applied: 3,
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
    let (mut runtime, _fake, client) = runtime_with_fake();
    let pane = only_pane(&runtime);
    feed_scrollback(&mut runtime, pane, 40);
    // Another client's queue, which would catch an answer sent to the wrong one.
    let queue = runtime.subscribe(ClientId::new(), EventFilter::All);

    runtime.run_client_mouse(
        client,
        14,
        vec![WireMouseAction::Scroll {
            pane,
            up: true,
            lines: 5,
        }],
    );

    assert_eq!(
        scroll_offset(&runtime, client, pane),
        5,
        "the round still ran"
    );
    assert_eq!(answers(&queue), Vec::new(), "and nothing was answered");
}

#[test]
fn a_two_cell_drag_moves_the_border_as_far_as_two_one_cell_drags_do() {
    // Five stacked panes on a 40-row viewport, on the stock 1-row pane
    // minimum. The donating pane's solved height does not change on the first
    // of the two cells, so the layout still has a cell to give after that
    // first one and only the second cell moves the border. Asking once and
    // retrying once for the spare stops a cell short here; asking again for
    // each fresh spare does not.
    let (mut runtime, _fake, client) = runtime_sized(Size { cols: 80, rows: 40 });
    for _ in 0..4 {
        split_focused_vertical(&mut runtime, client);
    }
    let before = stacked_panes(&runtime, client);
    assert_eq!(
        heights(&before),
        vec![19, 9, 4, 3, 3],
        "the five panes start at the heights this case needs"
    );

    let applied = runtime.drag_resize(client, before[3].0, Direction::Up, 1, 2);

    assert_eq!(applied, 2, "both cells of the drag were taken");
    assert_eq!(
        heights(&stacked_panes(&runtime, client)),
        vec![19, 9, 3, 3, 4],
        "the border landed where two one-cell drags put it"
    );
}
