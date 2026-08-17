//! Tests for the viewer's wheel decision: the precedence over one pane, which
//! pane a tick targets, and the ticks that decide nothing.
//!
//! Each test builds a frame by hand — one tab, one or two panes, no chrome
//! beyond the tabline and hint bar — so a case can be set up that a live
//! session would take many steps to reach.

use super::*;

use std::sync::mpsc;

use koshi_config::layer::{PartialKoshiConfig, PartialMouseConfig};
use koshi_config::types::WheelScroll;
use koshi_core::event::{Event, MouseSelectChanged};
use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, PluginId, SessionId, TabId};
use koshi_core::key::ModFlags;
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseTracking};
use koshi_layout::mode::LayoutMode;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::snapshot::{
    ClientSnapshot, Delivery, MousePane, PaneSlot, SessionSnapshot, TabMeta, TabSnapshot,
};

use crate::Client;

/// The frame size every fixture below is built at.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A viewer on the stock settings — `scroll_lines` 3, `wheel` scroll-scrollback.
fn viewer() -> Client {
    let (_tx, rx) = mpsc::sync_channel(8);
    Client::new(ClientId::new(), VIEWPORT, rx, TerminalCleanupGuard::new())
}

/// A viewer whose mouse-select mode the session turned on, reported the way
/// the running binary reports it — through the viewer's own subscription.
fn viewer_grabbing_the_mouse() -> Client {
    let (tx, rx) = mpsc::sync_channel(8);
    let mut viewer = Client::new(ClientId::new(), VIEWPORT, rx, TerminalCleanupGuard::new());
    tx.send(Delivery::Event(Event::MouseSelectChanged(
        MouseSelectChanged {
            client_id: viewer.id(),
            on: true,
        },
    )))
    .expect("the viewer's queue has room");
    viewer.apply_events();
    viewer
}

/// The same viewer with its `mouse` settings overridden.
fn viewer_with(mouse: PartialMouseConfig) -> Client {
    let mut viewer = viewer();
    viewer.load_startup_config(
        Some(PartialKoshiConfig {
            mouse: Some(mouse),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );
    viewer
}

/// One pane in a fixture frame: a plain terminal pane, no highlight, no mouse
/// mode, on the primary screen.
fn plain_pane(id: PaneId) -> MousePane {
    MousePane {
        id,
        view_top_row: 0,
        mouse_tracking: MouseTracking::Off,
        alt_scroll: false,
        on_alt_screen: false,
        has_selection: false,
    }
}

/// A frame of `panes`, laid out as full-width horizontal bands between the
/// tabline (row 0) and the hint bar (last row), with `focused` focused.
///
/// Two panes in an 80x24 viewport gives band rows 1..=11 and 12..=22, each with
/// a one-cell border ring, so `content_cell(0)` lands inside the first pane's
/// content and `content_cell(1)` inside the second's.
fn frame(panes: &[MousePane], focused: Option<PaneId>, kind: PaneKind) -> MouseFrame {
    let tab_id = TabId::new();
    let band = (VIEWPORT.rows - 2) / u16::try_from(panes.len()).expect("few panes");
    let layout_solved: Vec<PaneSlot> = panes
        .iter()
        .enumerate()
        .map(|(index, pane)| {
            let top = 1 + band * u16::try_from(index).expect("few panes");
            let rect = Rect::new(
                Point { x: 0, y: top },
                Size {
                    cols: VIEWPORT.cols,
                    rows: band,
                },
            );
            PaneSlot {
                pane_id: pane.id,
                rect,
                inner_rect: Some(Rect::new(
                    Point { x: 1, y: top + 1 },
                    Size {
                        cols: VIEWPORT.cols - 2,
                        rows: band - 2,
                    },
                )),
                kind: kind.clone(),
                visible: true,
                suppressed: false,
                dead: false,
            }
        })
        .collect();
    MouseFrame {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: "fixture".to_owned(),
            active_tab: TabSnapshot {
                id: tab_id,
                name: "one".to_owned(),
                layout_solved,
                effective_size: VIEWPORT,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
            },
            tabs_metadata: vec![TabMeta {
                id: tab_id,
                name: "one".to_owned(),
                index: 0,
                active: true,
            }],
        },
        panes: panes.to_vec(),
        client: ClientSnapshot {
            id: ClientId::new(),
            viewport: VIEWPORT,
            active_tab: tab_id,
            focused_pane: focused,
            lock_mode: LockMode::Normal,
            mouse_select: false,
        },
    }
}

/// A frame holding one plain terminal pane, focused.
fn one_pane_frame(pane: MousePane) -> MouseFrame {
    let id = pane.id;
    frame(&[pane], Some(id), PaneKind::Terminal)
}

/// A cell inside the content of the `index`-th pane in a fixture frame.
fn content_cell(frame: &MouseFrame, index: usize) -> Point {
    let inner = frame.session.active_tab.layout_solved[index]
        .inner_rect
        .expect("a visible pane");
    Point {
        x: inner.origin.x + 1,
        y: inner.origin.y + 1,
    }
}

/// A wheel tick at `at`.
fn wheel(direction: ScrollDirection, at: Point) -> MouseInput {
    MouseInput {
        kind: MouseKind::Scroll(direction),
        at,
        mods: ModFlags::NONE,
    }
}

#[test]
fn a_wheel_over_a_plain_pane_scrolls_by_the_viewers_own_line_count() {
    let pane = PaneId::new();
    let frame = one_pane_frame(plain_pane(pane));
    let mut viewer = viewer_with(PartialMouseConfig {
        scroll_lines: Some(7),
        ..PartialMouseConfig::default()
    });

    let decision = viewer
        .handle_mouse_wheel(wheel(ScrollDirection::Up, content_cell(&frame, 0)), &frame)
        .expect("a wheel tick decides");

    assert_eq!(decision.hovered, Some(pane));
    assert_eq!(
        decision.action,
        Some(MouseAction::Scroll {
            pane,
            up: true,
            lines: 7,
        })
    );
}

#[test]
fn a_wheel_down_over_a_plain_pane_moves_the_view_toward_live() {
    let pane = PaneId::new();
    let frame = one_pane_frame(plain_pane(pane));

    let decision = viewer()
        .handle_mouse_wheel(
            wheel(ScrollDirection::Down, content_cell(&frame, 0)),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        decision.action,
        Some(MouseAction::Scroll {
            pane,
            up: false,
            lines: 3,
        })
    );
}

#[test]
fn a_horizontal_wheel_over_a_plain_pane_decides_nothing() {
    let pane = PaneId::new();
    let frame = one_pane_frame(plain_pane(pane));

    for direction in [ScrollDirection::Left, ScrollDirection::Right] {
        let decision = viewer()
            .handle_mouse_wheel(wheel(direction, content_cell(&frame, 0)), &frame)
            .expect("a wheel tick decides");

        assert_eq!(decision.hovered, Some(pane), "{direction:?} still hovers");
        assert_eq!(
            decision.action, None,
            "{direction:?} moves no vertical view"
        );
    }
}

#[test]
fn the_ignore_setting_leaves_a_plain_pane_alone() {
    let pane = PaneId::new();
    let frame = one_pane_frame(plain_pane(pane));
    let mut viewer = viewer_with(PartialMouseConfig {
        wheel: Some(WheelScroll::Ignore),
        ..PartialMouseConfig::default()
    });

    let decision = viewer
        .handle_mouse_wheel(wheel(ScrollDirection::Up, content_cell(&frame, 0)), &frame)
        .expect("a wheel tick decides");

    assert_eq!(decision.action, None);
}

#[test]
fn a_program_asking_for_the_mouse_gets_the_tick_forwarded() {
    let pane = PaneId::new();
    let mut content = plain_pane(pane);
    content.mouse_tracking = MouseTracking::Normal;
    let frame = one_pane_frame(content);
    let tick = wheel(ScrollDirection::Up, content_cell(&frame, 0));

    let decision = viewer()
        .handle_mouse_wheel(tick, &frame)
        .expect("a wheel tick decides");

    assert_eq!(
        decision.action,
        Some(MouseAction::Forward { pane, mouse: tick })
    );
}

#[test]
fn x10_tracking_predates_the_wheel_so_the_tick_is_koshis() {
    // `?9` reports presses only. A wheel tick there is not the program's, so it
    // falls through to koshi's own scrollback.
    let pane = PaneId::new();
    let mut content = plain_pane(pane);
    content.mouse_tracking = MouseTracking::X10;
    let frame = one_pane_frame(content);

    let decision = viewer()
        .handle_mouse_wheel(wheel(ScrollDirection::Up, content_cell(&frame, 0)), &frame)
        .expect("a wheel tick decides");

    assert_eq!(
        decision.action,
        Some(MouseAction::Scroll {
            pane,
            up: true,
            lines: 3,
        })
    );
}

#[test]
fn a_highlight_holds_the_view_even_over_a_mouse_reporting_program() {
    let pane = PaneId::new();
    let mut content = plain_pane(pane);
    content.mouse_tracking = MouseTracking::Normal;
    content.has_selection = true;
    let frame = one_pane_frame(content);

    let decision = viewer()
        .handle_mouse_wheel(wheel(ScrollDirection::Up, content_cell(&frame, 0)), &frame)
        .expect("a wheel tick decides");

    assert_eq!(
        decision.action,
        Some(MouseAction::Scroll {
            pane,
            up: true,
            lines: 3,
        }),
        "the highlight wins over the program's mouse mode"
    );
}

#[test]
fn the_alternate_screen_with_alt_scroll_becomes_arrow_keys() {
    let pane = PaneId::new();
    let mut content = plain_pane(pane);
    content.on_alt_screen = true;
    content.alt_scroll = true;
    let frame = one_pane_frame(content);

    let up = viewer()
        .handle_mouse_wheel(wheel(ScrollDirection::Up, content_cell(&frame, 0)), &frame)
        .expect("a wheel tick decides");
    assert_eq!(
        up.action,
        Some(MouseAction::AltScrollArrows {
            pane,
            up: true,
            count: 3,
        })
    );

    let down = viewer()
        .handle_mouse_wheel(
            wheel(ScrollDirection::Down, content_cell(&frame, 0)),
            &frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(
        down.action,
        Some(MouseAction::AltScrollArrows {
            pane,
            up: false,
            count: 3,
        })
    );
}

#[test]
fn alt_scroll_off_the_alternate_screen_is_not_arrow_keys() {
    // `?1007` only translates the wheel while the alternate screen is up.
    let pane = PaneId::new();
    let mut content = plain_pane(pane);
    content.alt_scroll = true;
    let frame = one_pane_frame(content);

    let decision = viewer()
        .handle_mouse_wheel(wheel(ScrollDirection::Up, content_cell(&frame, 0)), &frame)
        .expect("a wheel tick decides");

    assert_eq!(
        decision.action,
        Some(MouseAction::Scroll {
            pane,
            up: true,
            lines: 3,
        })
    );
}

#[test]
fn a_horizontal_wheel_under_alt_scroll_sends_no_arrows() {
    let pane = PaneId::new();
    let mut content = plain_pane(pane);
    content.on_alt_screen = true;
    content.alt_scroll = true;
    let frame = one_pane_frame(content);

    let decision = viewer()
        .handle_mouse_wheel(
            wheel(ScrollDirection::Left, content_cell(&frame, 0)),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(decision.action, None);
}

#[test]
fn the_tick_targets_the_pane_under_the_pointer_not_the_focused_one() {
    let focused = PaneId::new();
    let other = PaneId::new();
    let frame = frame(
        &[plain_pane(focused), plain_pane(other)],
        Some(focused),
        PaneKind::Terminal,
    );

    let decision = viewer()
        .handle_mouse_wheel(wheel(ScrollDirection::Up, content_cell(&frame, 1)), &frame)
        .expect("a wheel tick decides");

    assert_eq!(decision.hovered, Some(other));
    assert_eq!(
        decision.action,
        Some(MouseAction::Scroll {
            pane: other,
            up: true,
            lines: 3,
        })
    );
}

#[test]
fn a_tick_over_chrome_falls_through_to_the_focused_pane_and_hovers_nothing() {
    let focused = PaneId::new();
    let frame = one_pane_frame(plain_pane(focused));

    // The hint bar on the bottom row is chrome: no pane sits under the pointer.
    let decision = viewer()
        .handle_mouse_wheel(
            wheel(
                ScrollDirection::Up,
                Point {
                    x: 40,
                    y: VIEWPORT.rows - 1,
                },
            ),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(decision.hovered, None, "chrome hovers no pane");
    assert_eq!(
        decision.action,
        Some(MouseAction::Scroll {
            pane: focused,
            up: true,
            lines: 3,
        })
    );
}

#[test]
fn a_tick_over_chrome_with_a_plugin_pane_focused_decides_nothing() {
    let focused = PaneId::new();
    let frame = frame(
        &[plain_pane(focused)],
        Some(focused),
        PaneKind::Plugin {
            plugin_id: PluginId::new(),
        },
    );

    let decision = viewer()
        .handle_mouse_wheel(
            wheel(
                ScrollDirection::Up,
                Point {
                    x: 40,
                    y: VIEWPORT.rows - 1,
                },
            ),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(decision.action, None, "a plugin pane runs no program");
}

#[test]
fn a_tick_over_the_tabline_steps_the_viewers_own_strip() {
    let frame = one_pane_frame(plain_pane(PaneId::new()));
    let tab = frame.client.active_tab;

    let mut viewer = viewer();
    let down = viewer
        .handle_mouse_wheel(wheel(ScrollDirection::Down, Point { x: 40, y: 0 }), &frame)
        .expect("a wheel tick decides");
    assert_eq!(down.hovered, None);
    assert_eq!(down.action, None, "the strip is the viewer's own to move");
    assert_eq!(viewer.chrome(tab).tabline_offset, Some(1));

    let up = viewer
        .handle_mouse_wheel(wheel(ScrollDirection::Up, Point { x: 40, y: 0 }), &frame)
        .expect("a wheel tick decides");
    assert_eq!(up.action, None);
    assert_eq!(
        viewer.chrome(tab).tabline_offset,
        Some(0),
        "the first visible index saturates at zero"
    );
}

#[test]
fn a_tab_switch_cancels_the_strip_peek() {
    let frame = one_pane_frame(plain_pane(PaneId::new()));
    let tab = frame.client.active_tab;
    let mut viewer = viewer();

    viewer
        .handle_mouse_wheel(wheel(ScrollDirection::Down, Point { x: 40, y: 0 }), &frame)
        .expect("a wheel tick decides");
    assert_eq!(viewer.chrome(tab).tabline_offset, Some(1));

    assert_eq!(
        viewer.chrome(TabId::new()).tabline_offset,
        None,
        "a peek belongs to the tab it was made on"
    );
}

#[test]
fn switching_away_and_back_does_not_bring_the_peek_out_again() {
    // The peek scrolled the strip away from the active tab. Seeing a frame on
    // another tab throws it away for good, so coming back to the tab it was
    // made on starts from that tab rather than putting the strip back where it
    // was — which could leave the active tab off the end of the strip.
    let frame = one_pane_frame(plain_pane(PaneId::new()));
    let first_tab = frame.client.active_tab;
    let other_tab = TabId::new();
    let mut viewer = viewer();

    viewer
        .handle_mouse_wheel(wheel(ScrollDirection::Down, Point { x: 40, y: 0 }), &frame)
        .expect("a wheel tick decides");
    assert_eq!(viewer.chrome(first_tab).tabline_offset, Some(1));

    viewer.note_active_tab(other_tab);
    assert_eq!(viewer.chrome(other_tab).tabline_offset, None);

    viewer.note_active_tab(first_tab);
    assert_eq!(
        viewer.chrome(first_tab).tabline_offset,
        None,
        "the peek was thrown away on the switch, not just ignored"
    );
}

#[test]
fn a_mouse_event_on_another_tabs_frame_throws_the_peek_away() {
    // The viewer also learns the tab from the frame a mouse event is answered
    // against, so a peek made on one tab does not come back after an event on
    // another.
    let frame = one_pane_frame(plain_pane(PaneId::new()));
    let first_tab = frame.client.active_tab;
    let mut other = one_pane_frame(plain_pane(PaneId::new()));
    other.client.active_tab = TabId::new();
    let mut viewer = viewer();

    viewer
        .handle_mouse_wheel(wheel(ScrollDirection::Down, Point { x: 40, y: 0 }), &frame)
        .expect("a wheel tick decides");
    assert_eq!(viewer.chrome(first_tab).tabline_offset, Some(1));

    viewer.handle_mouse(
        MouseInput {
            kind: MouseKind::Motion,
            at: content_cell(&other, 0),
            mods: ModFlags::NONE,
        },
        &other,
        Instant::now(),
    );

    assert_eq!(viewer.chrome(first_tab).tabline_offset, None);
}

#[test]
fn every_kind_but_the_wheel_is_left_to_the_session() {
    let frame = one_pane_frame(plain_pane(PaneId::new()));
    let at = content_cell(&frame, 0);

    for kind in [
        MouseKind::Press(MouseButton::Left),
        MouseKind::Press(MouseButton::Middle),
        MouseKind::Press(MouseButton::Right),
        MouseKind::Release(MouseButton::Left),
        MouseKind::Drag(MouseButton::Left),
        MouseKind::Motion,
    ] {
        assert_eq!(
            viewer().handle_mouse_wheel(
                MouseInput {
                    kind,
                    at,
                    mods: ModFlags::NONE,
                },
                &frame,
            ),
            None,
            "{kind:?} is not the viewer's to answer"
        );
    }
}

#[test]
fn a_tick_over_chrome_with_nothing_focused_decides_nothing() {
    // A tab with no focusable pane leaves a tick over chrome with no target.
    let frame = frame(&[plain_pane(PaneId::new())], None, PaneKind::Terminal);

    let decision = viewer()
        .handle_mouse_wheel(
            wheel(
                ScrollDirection::Up,
                Point {
                    x: 40,
                    y: VIEWPORT.rows - 1,
                },
            ),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(decision.hovered, None);
    assert_eq!(decision.action, None);
}

#[test]
fn a_tick_over_chrome_ignores_a_focused_pane_the_layout_does_not_place() {
    // The focused pane is not among this tab's solved slots, so nothing is
    // drawn for it and a tick that fell through to it targets nothing.
    let mut frame = one_pane_frame(plain_pane(PaneId::new()));
    frame.client.focused_pane = Some(PaneId::new());

    let decision = viewer()
        .handle_mouse_wheel(
            wheel(
                ScrollDirection::Up,
                Point {
                    x: 40,
                    y: VIEWPORT.rows - 1,
                },
            ),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(decision.action, None);
}

#[test]
fn a_zero_line_scroll_setting_is_carried_through_as_zero() {
    // `scroll_lines 0` is the user asking a notch to move nothing. It is
    // carried as a zero-line movement rather than falling back to a default.
    let pane = PaneId::new();
    let frame = one_pane_frame(plain_pane(pane));
    let mut viewer = viewer_with(PartialMouseConfig {
        scroll_lines: Some(0),
        ..PartialMouseConfig::default()
    });

    let decision = viewer
        .handle_mouse_wheel(wheel(ScrollDirection::Up, content_cell(&frame, 0)), &frame)
        .expect("a wheel tick decides");

    assert_eq!(
        decision.action,
        Some(MouseAction::Scroll {
            pane,
            up: true,
            lines: 0,
        })
    );
}

#[test]
fn a_frame_with_no_room_falls_through_to_the_focused_pane() {
    // Every pane is suppressed, so the frame is the "terminal too small"
    // overlay: no tab strip and no pane content is drawn, and nothing is
    // hit-testable.
    let pane = PaneId::new();
    let mut frame = one_pane_frame(plain_pane(pane));
    frame.session.active_tab.all_suppressed = true;

    for at in [
        Point { x: 40, y: 0 },
        Point { x: 40, y: 10 },
        Point {
            x: 40,
            y: VIEWPORT.rows - 1,
        },
    ] {
        let decision = viewer()
            .handle_mouse_wheel(wheel(ScrollDirection::Down, at), &frame)
            .expect("a wheel tick decides");

        assert_eq!(decision.hovered, None, "{at:?} hovers no pane");
        assert_eq!(
            decision.action,
            Some(MouseAction::Scroll {
                pane,
                up: false,
                lines: 3,
            }),
            "{at:?} falls through to the focused pane, and never to the tab strip"
        );
    }
}

#[test]
fn a_tick_aimed_at_a_pane_the_frame_carries_no_content_for_decides_nothing() {
    // The slot is laid out but no `PaneSnapshot` came with it, so nothing is
    // known about the pane's modes and no decision can be made about it.
    let pane = PaneId::new();
    let mut frame = one_pane_frame(plain_pane(pane));
    frame.panes.clear();

    let decision = viewer()
        .handle_mouse_wheel(wheel(ScrollDirection::Up, content_cell(&frame, 0)), &frame)
        .expect("a wheel tick decides");

    assert_eq!(decision.hovered, Some(pane), "the layout still places it");
    assert_eq!(decision.action, None);
}

// ============================================================================
// Gestures other than the wheel
// ============================================================================

/// A press, drag, or release of the left button at `at`, with `mods` held.
fn event(kind: MouseKind, at: Point, mods: ModFlags) -> MouseInput {
    MouseInput { kind, at, mods }
}

/// A left press at `at`.
fn press(at: Point) -> MouseInput {
    event(MouseKind::Press(MouseButton::Left), at, ModFlags::NONE)
}

/// A left drag to `at`.
fn drag(at: Point) -> MouseInput {
    event(MouseKind::Drag(MouseButton::Left), at, ModFlags::NONE)
}

/// A left release at `at`.
fn release(at: Point) -> MouseInput {
    event(MouseKind::Release(MouseButton::Left), at, ModFlags::NONE)
}

/// A buttonless move to `at`.
fn motion(at: Point) -> MouseInput {
    event(MouseKind::Motion, at, ModFlags::NONE)
}

/// An instant a second after `base`, so two presses never read as a double
/// click.
fn later(base: Instant, secs: u64) -> Instant {
    base + Duration::from_secs(secs)
}

/// The single `SetSelection` in `actions`, or `None` when it holds none.
fn set_selection(actions: &[MouseAction]) -> Option<SetSelectionArgs> {
    actions.iter().find_map(|action| match action {
        MouseAction::Command(Command::Visual(VisualCommand::SetSelection(args))) => Some(*args),
        _ => None,
    })
}

/// The single `Copy` in `actions`, or `None` when it holds none.
fn copy(actions: &[MouseAction]) -> Option<CopyArgs> {
    actions.iter().find_map(|action| match action {
        MouseAction::Command(Command::Visual(VisualCommand::Copy(args))) => Some(*args),
        _ => None,
    })
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
fn advance_along_walks_the_anchor_the_way_the_answered_step_asked_and_saturates() {
    let from = Point { x: 3, y: 3 };
    // A positive step grows the pane: a right or down border walks away from
    // zero, a left or up border walks toward it.
    assert_eq!(
        advance_along(Direction::Right, from, 1, 2),
        Point { x: 5, y: 3 }
    );
    assert_eq!(
        advance_along(Direction::Left, from, 1, 2),
        Point { x: 1, y: 3 }
    );
    assert_eq!(
        advance_along(Direction::Down, from, 1, 2),
        Point { x: 3, y: 5 }
    );
    assert_eq!(
        advance_along(Direction::Up, from, 1, 2),
        Point { x: 3, y: 1 }
    );
    // A negative step shrinks it, so every border walks the other way.
    assert_eq!(
        advance_along(Direction::Right, from, -1, 2),
        Point { x: 1, y: 3 }
    );
    assert_eq!(
        advance_along(Direction::Left, from, -1, 2),
        Point { x: 5, y: 3 }
    );
    assert_eq!(
        advance_along(Direction::Down, from, -1, 2),
        Point { x: 3, y: 1 }
    );
    assert_eq!(
        advance_along(Direction::Up, from, -1, 2),
        Point { x: 3, y: 5 }
    );
    // The anchor lands exactly where `resize_delta` reads the move back.
    for side in [
        Direction::Left,
        Direction::Right,
        Direction::Up,
        Direction::Down,
    ] {
        for step in [-1, 1] {
            assert_eq!(
                resize_delta(side, from, advance_along(side, from, step, 2)),
                step * 2,
                "{side:?} answered a step of {step}"
            );
        }
    }
    // Saturating: an anchor at an edge cannot wrap past either end.
    assert_eq!(
        advance_along(Direction::Left, from, 1, 10),
        Point { x: 0, y: 3 }
    );
    assert_eq!(
        advance_along(Direction::Up, from, 1, 10),
        Point { x: 3, y: 0 }
    );
    assert_eq!(
        advance_along(
            Direction::Right,
            Point {
                x: u16::MAX - 1,
                y: 3
            },
            1,
            5
        ),
        Point { x: u16::MAX, y: 3 }
    );
}

#[test]
fn a_press_names_the_line_the_frame_showed_on_that_row() {
    // The load-bearing claim of absolute anchoring: the frame says which line
    // the pane's top visible row is, so the press names that line plus the row
    // it landed on — whatever the pane's live view has done since.
    let pane = PaneId::new();
    let mut content = plain_pane(pane);
    content.view_top_row = 940;
    let frame = one_pane_frame(content);
    let mut viewer = viewer();
    let now = Instant::now();

    // The second visible row of the pane's content.
    let inner = frame.session.active_tab.layout_solved[0]
        .inner_rect
        .expect("a visible pane");
    let at = Point {
        x: inner.origin.x + 4,
        y: inner.origin.y + 2,
    };
    viewer.handle_mouse(press(at), &frame, now);
    let actions = viewer.handle_mouse(
        drag(Point {
            x: inner.origin.x + 9,
            y: inner.origin.y + 2,
        }),
        &frame,
        later(now, 1),
    );

    let args = set_selection(&actions).expect("the drag asked for a highlight");
    assert_eq!(args.pane, pane);
    assert_eq!(args.selection.anchor, GridPos { row: 942, col: 4 });
    assert_eq!(args.selection.cursor, GridPos { row: 942, col: 9 });
}

#[test]
fn output_between_the_paint_and_the_press_does_not_move_what_it_names() {
    // The same press against the same painted frame names the same line even
    // once the pane has pushed more output: the frame's line numbers are
    // absolute, so nothing about them shifts.
    let pane = PaneId::new();
    let mut scrolled = plain_pane(pane);
    scrolled.view_top_row = 500;
    let painted = one_pane_frame(scrolled);
    let inner = painted.session.active_tab.layout_solved[0]
        .inner_rect
        .expect("a visible pane");
    let at = Point {
        x: inner.origin.x,
        y: inner.origin.y + 3,
    };
    let now = Instant::now();

    let mut viewer = viewer();
    viewer.handle_mouse(press(at), &painted, now);
    let actions = viewer.handle_mouse(
        drag(Point {
            x: inner.origin.x + 2,
            y: inner.origin.y + 3,
        }),
        &painted,
        later(now, 1),
    );

    assert_eq!(
        set_selection(&actions)
            .expect("a highlight")
            .selection
            .anchor,
        GridPos { row: 503, col: 0 },
        "the press names the line the user saw on that row"
    );
}

#[test]
fn a_second_press_inside_the_threshold_selects_whole_words() {
    let pane = PaneId::new();
    let frame = one_pane_frame(plain_pane(pane));
    let at = content_cell(&frame, 0);
    let mut viewer = viewer();
    let now = Instant::now();

    viewer.handle_mouse(press(at), &frame, now);
    let actions = viewer.handle_mouse(press(at), &frame, now + Duration::from_millis(120));

    assert_eq!(
        set_selection(&actions)
            .expect("a word highlight")
            .selection
            .kind,
        SelectionKind::Word,
        "the second press in the run names a word"
    );
}

#[test]
fn a_third_press_selects_whole_lines_and_a_fourth_starts_over() {
    let pane = PaneId::new();
    let frame = one_pane_frame(plain_pane(pane));
    let at = content_cell(&frame, 0);
    let mut viewer = viewer();
    let now = Instant::now();

    viewer.handle_mouse(press(at), &frame, now);
    viewer.handle_mouse(press(at), &frame, now + Duration::from_millis(100));
    let third = viewer.handle_mouse(press(at), &frame, now + Duration::from_millis(200));
    assert_eq!(
        set_selection(&third)
            .expect("a line highlight")
            .selection
            .kind,
        SelectionKind::Line
    );

    // A fourth press starts the run over, and a single click names a point, so
    // it highlights nothing until the pointer moves.
    let fourth = viewer.handle_mouse(press(at), &frame, now + Duration::from_millis(300));
    assert_eq!(set_selection(&fourth), None, "the run began again");
    let dragged = viewer.handle_mouse(
        drag(Point { x: at.x + 3, ..at }),
        &frame,
        now + Duration::from_millis(320),
    );
    assert_eq!(
        set_selection(&dragged).expect("a highlight").selection.kind,
        SelectionKind::Character
    );
}

#[test]
fn a_press_past_the_threshold_is_another_single_click() {
    let pane = PaneId::new();
    let frame = one_pane_frame(plain_pane(pane));
    let at = content_cell(&frame, 0);
    let mut viewer = viewer();
    let now = Instant::now();

    viewer.handle_mouse(press(at), &frame, now);
    viewer.handle_mouse(press(at), &frame, now + Duration::from_millis(400));
    let actions = viewer.handle_mouse(
        drag(Point { x: at.x + 3, ..at }),
        &frame,
        now + Duration::from_millis(420),
    );

    assert_eq!(
        set_selection(&actions).expect("a highlight").selection.kind,
        SelectionKind::Character,
        "exactly 400ms is no longer inside the threshold"
    );
}

#[test]
fn alt_held_at_the_press_makes_a_block_whatever_the_run_of_clicks_was() {
    let pane = PaneId::new();
    let frame = one_pane_frame(plain_pane(pane));
    let at = content_cell(&frame, 0);
    let mut viewer = viewer();
    let now = Instant::now();

    viewer.handle_mouse(press(at), &frame, now);
    let actions = viewer.handle_mouse(
        event(MouseKind::Press(MouseButton::Left), at, ModFlags::ALT),
        &frame,
        now + Duration::from_millis(120),
    );
    // A block names a point, so the press alone highlights nothing.
    assert_eq!(set_selection(&actions), None);

    let dragged = viewer.handle_mouse(
        drag(Point { x: at.x + 2, ..at }),
        &frame,
        now + Duration::from_millis(140),
    );
    assert_eq!(
        set_selection(&dragged).expect("a highlight").selection.kind,
        SelectionKind::Block
    );
}

#[test]
fn a_captured_drag_that_leaves_the_pane_still_reaches_it_and_the_release_ends_it() {
    let pane = PaneId::new();
    let mut content = plain_pane(pane);
    // Button-event tracking reports presses, drags, and releases.
    content.mouse_tracking = MouseTracking::ButtonMotion;
    let frame = one_pane_frame(content);
    let at = content_cell(&frame, 0);
    let mut viewer = viewer();
    let now = Instant::now();

    let pressed = viewer.handle_mouse(press(at), &frame, now);
    assert_eq!(
        pressed,
        vec![MouseAction::Forward {
            pane,
            mouse: press(at),
        }]
    );
    // The session wrote that press to the pane, which is what captures the
    // gesture; the loop reports it back.
    viewer.note_press_forwarded(pane, MouseButton::Left);

    // Row 0 is the tabline — off the pane entirely. The capture still routes it.
    let outside = Point { x: 0, y: 0 };
    let dragged = viewer.handle_mouse(drag(outside), &frame, later(now, 1));
    assert_eq!(
        dragged,
        vec![MouseAction::Forward {
            pane,
            mouse: drag(outside),
        }],
        "the held button keeps the gesture on the pane it pressed"
    );

    let released = viewer.handle_mouse(release(outside), &frame, later(now, 2));
    assert_eq!(
        released,
        vec![MouseAction::Forward {
            pane,
            mouse: release(outside),
        }]
    );

    // The capture is over: a further drag reaches no program.
    assert_eq!(
        viewer.handle_mouse(drag(outside), &frame, later(now, 3)),
        Vec::new(),
        "a drag with no press behind it forwards nothing"
    );
}

#[test]
fn a_bare_move_off_the_focused_panes_content_reaches_no_program() {
    // No button is held, so there is no capture to route by: a move only
    // reaches a program when it lands inside that program's own content. Both
    // panes are drawn, the upper one is focused, and the pointer is in the
    // lower one.
    let focused = PaneId::new();
    let mut content = plain_pane(focused);
    // Any-event tracking asks for every move, so the position is the only thing
    // keeping this one out.
    content.mouse_tracking = MouseTracking::AnyMotion;
    let frame = frame(
        &[content, plain_pane(PaneId::new())],
        Some(focused),
        PaneKind::Terminal,
    );
    let inside = content_cell(&frame, 0);
    let outside = content_cell(&frame, 1);
    let now = Instant::now();
    let mut viewer = viewer();

    assert_eq!(
        viewer.handle_mouse(motion(inside), &frame, now),
        vec![MouseAction::Forward {
            pane: focused,
            mouse: motion(inside),
        }],
        "a move inside the focused pane's content is the program's"
    );

    assert_eq!(
        viewer.handle_mouse(motion(outside), &frame, later(now, 1)),
        Vec::new(),
        "the same move one band down names no cell in the focused pane"
    );
}

#[test]
fn a_gesture_is_dropped_when_its_pane_leaves_the_frame() {
    let pane = PaneId::new();
    let frame = one_pane_frame(plain_pane(pane));
    let at = content_cell(&frame, 0);
    let mut viewer = viewer();
    let now = Instant::now();

    viewer.handle_mouse(press(at), &frame, now);

    // The pane closes: the next frame does not draw it.
    let other = PaneId::new();
    let gone = one_pane_frame(plain_pane(other));
    let actions = viewer.handle_mouse(drag(content_cell(&gone, 0)), &gone, later(now, 1));

    assert_eq!(
        actions,
        Vec::new(),
        "the drag's pane is not on the frame, so the gesture ended with it"
    );
}

#[test]
fn a_border_press_starts_no_resize_when_the_viewer_turned_it_off() {
    let left = PaneId::new();
    let right = PaneId::new();
    let frame = frame(
        &[plain_pane(left), plain_pane(right)],
        Some(left),
        PaneKind::Terminal,
    );
    // The shared divider between the two bands: the second pane's top edge.
    let divider = Point {
        x: 10,
        y: frame.session.active_tab.layout_solved[1].rect.origin.y,
    };
    let now = Instant::now();

    let mut off = viewer_with(PartialMouseConfig {
        border_resize: Some(false),
        ..PartialMouseConfig::default()
    });
    off.handle_mouse(press(divider), &frame, now);
    assert_eq!(
        off.handle_mouse(
            drag(Point {
                y: divider.y + 3,
                ..divider
            }),
            &frame,
            later(now, 1)
        ),
        Vec::new(),
        "border resize is off, so the drag moves nothing"
    );

    // The same gesture with the setting on does move the border.
    let mut on = viewer();
    on.handle_mouse(press(divider), &frame, now);
    let actions = on.handle_mouse(
        drag(Point {
            y: divider.y + 3,
            ..divider
        }),
        &frame,
        later(now, 1),
    );
    assert_eq!(
        actions,
        vec![MouseAction::Resize {
            pane: right,
            side: Direction::Up,
            step: -1,
            count: 3,
        }],
        "three cells of travel away from the grabbed top border"
    );
}

#[test]
fn grabbing_a_border_with_no_pane_beside_it_starts_no_resize() {
    let pane = PaneId::new();
    let frame = one_pane_frame(plain_pane(pane));
    // The pane's own left edge: the tab's outer frame, with nothing beyond it.
    let outer = Point {
        x: frame.session.active_tab.layout_solved[0].rect.origin.x,
        y: 5,
    };
    let mut viewer = viewer();
    let now = Instant::now();

    viewer.handle_mouse(press(outer), &frame, now);

    assert_eq!(
        viewer.handle_mouse(
            drag(Point {
                x: outer.x + 3,
                ..outer
            }),
            &frame,
            later(now, 1)
        ),
        Vec::new(),
        "the tab's outer frame has no neighbour to resize against"
    );
}

#[test]
fn releasing_a_highlight_copies_it_with_the_viewers_own_trim_setting() {
    let pane = PaneId::new();
    let frame = one_pane_frame(plain_pane(pane));
    let at = content_cell(&frame, 0);

    for trim in [true, false] {
        let mut viewer = viewer();
        viewer.config.copy.trim_trailing_whitespace = trim;
        let now = Instant::now();

        viewer.handle_mouse(press(at), &frame, now);
        viewer.handle_mouse(drag(Point { x: at.x + 4, ..at }), &frame, later(now, 1));
        let actions =
            viewer.handle_mouse(release(Point { x: at.x + 4, ..at }), &frame, later(now, 2));

        assert_eq!(
            copy(&actions).expect("the release is the copy"),
            CopyArgs {
                pane,
                target: CopyTarget::Osc52,
                trim_trailing_whitespace: trim,
            },
            "trim {trim}"
        );
    }
}

#[test]
fn copy_on_select_off_releases_without_copying() {
    let pane = PaneId::new();
    let frame = one_pane_frame(plain_pane(pane));
    let at = content_cell(&frame, 0);
    let mut viewer = viewer();
    viewer.config.copy.copy_on_select = false;
    let now = Instant::now();

    viewer.handle_mouse(press(at), &frame, now);
    viewer.handle_mouse(drag(Point { x: at.x + 4, ..at }), &frame, later(now, 1));
    let actions = viewer.handle_mouse(release(Point { x: at.x + 4, ..at }), &frame, later(now, 2));

    assert_eq!(copy(&actions), None, "the highlight stands, uncopied");
}

#[test]
fn a_drag_held_past_the_bottom_edge_scrolls_on_the_clock() {
    let pane = PaneId::new();
    let mut content = plain_pane(pane);
    content.view_top_row = 100;
    let frame = one_pane_frame(content);
    let at = content_cell(&frame, 0);
    let inner = frame.session.active_tab.layout_solved[0]
        .inner_rect
        .expect("a visible pane");
    let below = Point {
        x: at.x,
        y: inner.origin.y + inner.size.rows + 5,
    };
    let mut viewer = viewer();
    let now = Instant::now();

    viewer.handle_mouse(press(at), &frame, now);
    viewer.handle_mouse(drag(below), &frame, later(now, 1));
    let due = later(now, 1) + Duration::from_millis(15);
    assert_eq!(
        viewer.next_mouse_wakeup(later(now, 1)),
        Some(Duration::from_millis(15)),
        "the pointer past the edge asks the loop to wake"
    );

    // The firing asks for one line of scroll and nothing else yet.
    let fired = viewer.expire_mouse_scroll(due, &frame);
    assert_eq!(
        fired,
        vec![MouseAction::Scroll {
            pane,
            up: false,
            lines: 1,
        }]
    );

    // The session says the view moved on by one line; the highlight follows it
    // to the pane's last row.
    let actions = viewer.note_scroll_applied(pane, Some(101), &frame);
    let args = set_selection(&actions).expect("the scroll re-extends the highlight");
    assert_eq!(
        args.selection.cursor,
        GridPos {
            row: 101 + u64::from(inner.size.rows - 1),
            col: at.x - inner.origin.x,
        },
        "the moving end sits on the last row of the view the scroll revealed"
    );
}

#[test]
fn a_scroll_that_moved_nothing_disarms_the_timer() {
    let pane = PaneId::new();
    let mut content = plain_pane(pane);
    content.view_top_row = 100;
    let frame = one_pane_frame(content);
    let at = content_cell(&frame, 0);
    let inner = frame.session.active_tab.layout_solved[0]
        .inner_rect
        .expect("a visible pane");
    let below = Point {
        x: at.x,
        y: inner.origin.y + inner.size.rows + 5,
    };
    let mut viewer = viewer();
    let now = Instant::now();

    viewer.handle_mouse(press(at), &frame, now);
    viewer.handle_mouse(drag(below), &frame, later(now, 1));
    let due = later(now, 1) + Duration::from_millis(15);
    viewer.expire_mouse_scroll(due, &frame);

    // The view was already at its limit: the top line did not move.
    let actions = viewer.note_scroll_applied(pane, Some(100), &frame);

    assert_eq!(actions, Vec::new(), "nothing revealed, nothing to extend");
    assert_eq!(
        viewer.next_mouse_wakeup(due),
        None,
        "a firing that moved nothing disarms the timer"
    );
}

#[test]
fn a_wheel_scroll_never_re_extends_a_highlight() {
    // `note_scroll_applied` answers only the scroll the edge timer asked for; a
    // wheel tick's scroll leaves it with nothing to do.
    let pane = PaneId::new();
    let frame = one_pane_frame(plain_pane(pane));
    let mut viewer = viewer();

    viewer.handle_mouse(
        wheel(ScrollDirection::Up, content_cell(&frame, 0)),
        &frame,
        Instant::now(),
    );

    assert_eq!(
        viewer.note_scroll_applied(pane, Some(7), &frame),
        Vec::new()
    );
}

#[test]
fn a_press_on_an_unfocused_pane_only_focuses_it() {
    let focused = PaneId::new();
    let other = PaneId::new();
    let frame = frame(
        &[plain_pane(focused), plain_pane(other)],
        Some(focused),
        PaneKind::Terminal,
    );
    let mut viewer = viewer();

    let actions = viewer.handle_mouse(press(content_cell(&frame, 1)), &frame, Instant::now());

    assert_eq!(
        actions,
        vec![MouseAction::Command(Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(other),
            client: Some(viewer.id()),
        }))],
        "the first click focuses and nothing else"
    );
}

#[test]
fn mouse_select_mode_takes_a_drag_back_from_a_mouse_aware_program() {
    let pane = PaneId::new();
    let mut content = plain_pane(pane);
    content.mouse_tracking = MouseTracking::ButtonMotion;
    let frame = one_pane_frame(content);
    let at = content_cell(&frame, 0);
    let now = Instant::now();

    // With mouse-select off the press is the program's.
    let mut plain = viewer();
    assert_eq!(
        plain.handle_mouse(press(at), &frame, now),
        vec![MouseAction::Forward {
            pane,
            mouse: press(at),
        }]
    );

    // With it on, the same press begins a koshi highlight instead.
    let mut grabbing = viewer_grabbing_the_mouse();
    let actions = grabbing.handle_mouse(press(at), &frame, now);
    assert_eq!(
        actions,
        vec![MouseAction::Command(Command::Visual(
            VisualCommand::ClearSelection(ClearSelectionArgs { pane })
        ))],
        "the press drops the old highlight and arms a drag"
    );
}

#[test]
fn ending_the_gestures_drops_all_four_and_leaves_the_pointer_and_the_strip_alone() {
    let first = PaneId::new();
    let second = PaneId::new();
    let frame = frame(
        &[plain_pane(first), plain_pane(second)],
        Some(first),
        PaneKind::Terminal,
    );
    let tab = frame.client.active_tab;
    let at = content_cell(&frame, 0);
    // The shared divider between the two bands: the second pane's top edge.
    let divider = Point {
        x: 10,
        y: frame.session.active_tab.layout_solved[1].rect.origin.y,
    };
    // The bare tab strip, past the one tab's ribbon.
    let strip = Point { x: 40, y: 0 };
    let now = Instant::now();
    let mut viewer = viewer();

    // A peeked strip and a hovered pane. Neither is a gesture.
    viewer
        .handle_mouse_wheel(wheel(ScrollDirection::Down, strip), &frame)
        .expect("a wheel tick decides");
    viewer.handle_mouse(motion(at), &frame, now);
    assert_eq!(viewer.chrome(tab).tabline_offset, Some(1));
    assert_eq!(viewer.chrome(tab).hovered_pane, Some(first));

    // All four gestures under way at once: the presses that began them are
    // spaced so no two read as one double click.
    viewer.handle_mouse(press(at), &frame, now);
    viewer.handle_mouse(press(divider), &frame, later(now, 1));
    viewer.handle_mouse(press(strip), &frame, later(now, 2));
    viewer.note_press_forwarded(second, MouseButton::Left);
    assert_eq!(viewer.selection_drag.map(|drag| drag.pane), Some(first));
    assert_eq!(viewer.resize_drag.map(|drag| drag.pane), Some(second));
    assert_eq!(viewer.tabline_drag.map(|drag| drag.anchor_x), Some(strip.x));
    assert_eq!(viewer.mouse_capture, Some((second, MouseButton::Left)));

    viewer.end_mouse_gestures();

    assert_eq!(viewer.selection_drag, None);
    assert_eq!(viewer.resize_drag, None);
    assert_eq!(viewer.tabline_drag, None);
    assert_eq!(viewer.mouse_capture, None);
    assert_eq!(
        viewer.chrome(tab).tabline_offset,
        Some(1),
        "the strip peek stands"
    );
    assert_eq!(
        viewer.chrome(tab).hovered_pane,
        Some(first),
        "the hovered pane stands"
    );
    assert_eq!(
        viewer.handle_mouse(drag(Point { x: at.x + 4, ..at }), &frame, later(now, 3)),
        Vec::new(),
        "no gesture is under way, so the drag decides nothing"
    );
}
