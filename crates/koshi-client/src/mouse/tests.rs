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
use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, PluginId, SessionId, TabId};
use koshi_core::key::ModFlags;
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseTracking};
use koshi_layout::mode::LayoutMode;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::snapshot::{
    ClientSnapshot, MousePane, PaneSlot, SessionSnapshot, TabMeta, TabSnapshot,
};

use crate::Client;

/// The frame size every fixture below is built at.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A viewer on the stock settings — `scroll_lines` 3, `wheel` scroll-scrollback.
fn viewer() -> Client {
    let (_tx, rx) = mpsc::sync_channel(8);
    Client::new(ClientId::new(), VIEWPORT, rx, TerminalCleanupGuard::new())
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
            hovered_pane: None,
            lock_mode: LockMode::Normal,
            mouse_select: false,
            tabline_offset: None,
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
    let viewer = viewer_with(PartialMouseConfig {
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
    let viewer = viewer_with(PartialMouseConfig {
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
fn a_tick_over_the_tabline_steps_the_strip() {
    let frame = one_pane_frame(plain_pane(PaneId::new()));

    let down = viewer()
        .handle_mouse_wheel(wheel(ScrollDirection::Down, Point { x: 40, y: 0 }), &frame)
        .expect("a wheel tick decides");
    assert_eq!(down.hovered, None);
    assert_eq!(down.action, Some(MouseAction::ScrollTabline { to: 1 }));

    let up = viewer()
        .handle_mouse_wheel(wheel(ScrollDirection::Up, Point { x: 40, y: 0 }), &frame)
        .expect("a wheel tick decides");
    assert_eq!(
        up.action,
        Some(MouseAction::ScrollTabline { to: 0 }),
        "the first visible index saturates at zero"
    );
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
    let viewer = viewer_with(PartialMouseConfig {
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
