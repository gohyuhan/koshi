//! Mouse routing tests: a tab click focuses the tab and clears the peek, a
//! scroll arrow and the wheel peek the strip, and a tabline drag scrolls it.
//!
//! Client state is read back through [`Runtime::build_snapshot`] — the same
//! projection the renderer draws — so a test never reaches into private client
//! fields.

use super::*;

use std::sync::{mpsc, Arc};

use koshi_core::command::NewTabArgs;
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
