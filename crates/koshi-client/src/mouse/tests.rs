//! Tests for what the viewer decides a mouse event means: the wheel's
//! precedence over one pane, which pane a tick targets, and the ticks that
//! decide nothing; what a press, a drag and a release do over each region —
//! focusing a tab or a pane, peeking the tab strip, moving a border, and
//! highlighting text; the run of clicks that picks a highlight's shape; the
//! capture that keeps a gesture with the pane its press landed on; the edge
//! scroll a held drag drives on the clock; and the gestures a new frame ends.
//!
//! Each test builds a frame by hand — one tab, one or two panes, no build_viewer_chrome
//! beyond the tabline and hint bar — so a case can be set up that a live
//! session would take many steps to reach.

use super::*;

use koshi_config::layer::{PartialKoshiConfig, PartialMouseConfig};
use koshi_config::types::WheelScroll;
use koshi_core::event::{Event, MouseSelectChanged};
use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, PluginId, SessionId, TabId};
use koshi_core::key::ModFlags;
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseTracking};
use koshi_layout::mode::LayoutMode;
use koshi_layout::regions::{solve_region_rects, Edge, RegionGeometry};
use koshi_renderer::snapshot::{
    ClientSnapshot, CommittedRegions, Delivery, MousePane, PaneSlot, SessionSnapshot, TabMeta,
    TabSnapshot,
};

use crate::tests::TEST_VIEWPORT_SIZE;
use crate::Client;

/// A viewer on the stock settings — `scroll_line_count` 3, `wheel` scroll-scrollback.
fn build_test_client() -> Client {
    crate::tests::build_test_client_with_event_sender().0
}

/// A viewer whose mouse-select mode the session turned on, reported the way
/// the running binary reports it — through the viewer's own subscription.
fn build_mouse_select_client() -> Client {
    let (mut client, event_sender) = crate::tests::build_test_client_with_event_sender();
    event_sender
        .send(Delivery::Event(Event::MouseSelectChanged(
            MouseSelectChanged {
                client_id: client.get_client_id(),
                is_enabled: true,
            },
        )))
        .expect("the client's queue has room");
    client.apply_events();
    client
}

/// The same viewer with its `mouse` settings overridden.
fn build_test_client_with_mouse_config(mouse_config: PartialMouseConfig) -> Client {
    let mut client = build_test_client();
    client.load_startup_config(
        Some(PartialKoshiConfig {
            mouse: Some(mouse_config),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );
    client
}

/// One pane in a fixture frame: a plain terminal pane, no highlight, no mouse
/// mode, on the primary screen.
fn build_plain_mouse_pane(pane_id: PaneId) -> MousePane {
    MousePane {
        pane_id,
        view_top_row_index: 0,
        mouse_tracking: MouseTracking::Off,
        is_alternate_scroll_enabled: false,
        is_on_alternate_screen: false,
        has_selection: false,
    }
}

/// A frame of `panes`, laid out as full-width horizontal bands between the
/// tabline (row 0) and the hint bar (last row), with `focused` focused.
///
/// Two panes in an 80x24 viewport gives band rows 1..=11 and 12..=22, each with
/// a one-cell border ring, so `get_content_cell(0)` lands inside the first pane's
/// content and `get_content_cell(1)` inside the second's.
fn build_mouse_frame(
    panes: &[MousePane],
    focused_pane_id: Option<PaneId>,
    pane_kind: PaneKind,
) -> MouseFrame {
    let tab_id = TabId::new();
    let band = (TEST_VIEWPORT_SIZE.row_count - 2) / u16::try_from(panes.len()).expect("few panes");
    let pane_slots: Vec<PaneSlot> = panes
        .iter()
        .enumerate()
        .map(|(pane_index, mouse_pane)| {
            let top_row = band * u16::try_from(pane_index).expect("few panes");
            let outer_rect = Rect::from_origin_and_size(
                Point {
                    column: 0,
                    row: top_row,
                },
                Size {
                    column_count: TEST_VIEWPORT_SIZE.column_count,
                    row_count: band,
                },
            );
            PaneSlot {
                pane_id: mouse_pane.pane_id,
                outer_rect,
                content_rect: Some(Rect::from_origin_and_size(
                    Point {
                        column: 1,
                        row: top_row + 1,
                    },
                    Size {
                        column_count: TEST_VIEWPORT_SIZE.column_count - 2,
                        row_count: band - 2,
                    },
                )),
                pane_kind,
                is_visible: true,
                is_suppressed: false,
                is_dead: false,
            }
        })
        .collect();
    MouseFrame {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_name: "fixture".to_owned(),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: "one".to_owned(),
                pane_slots,
                effective_cell_size: Size {
                    column_count: TEST_VIEWPORT_SIZE.column_count,
                    row_count: TEST_VIEWPORT_SIZE.row_count - 2,
                },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                are_all_panes_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: vec![TabMeta {
                tab_id,
                tab_name: "one".to_owned(),
                tab_index: 0,
                is_active: true,
            }],
        },
        mouse_panes: panes.to_vec(),
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            viewport_size: TEST_VIEWPORT_SIZE,
            active_tab_id: tab_id,
            focused_pane_id,
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
        },
        committed_regions: CommittedRegions::core(TEST_VIEWPORT_SIZE, 0),
    }
}

/// A frame holding one plain terminal pane, focused.
fn build_one_pane_mouse_frame(mouse_pane: MousePane) -> MouseFrame {
    let pane_id = mouse_pane.pane_id;
    build_mouse_frame(&[mouse_pane], Some(pane_id), PaneKind::Terminal)
}

/// A cell inside the content of the `pane_index`-th pane in a fixture frame.
fn get_content_cell(mouse_frame: &MouseFrame, pane_index: usize) -> Point {
    let content_rect = mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[pane_index]
        .content_rect
        .expect("a visible pane");
    Point {
        column: content_rect.origin.column + 1,
        row: content_rect.origin.row + 2,
    }
}

/// A wheel tick at `at`.
fn build_mouse_wheel(direction: ScrollDirection, position: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Scroll(direction),
        position,
        modifier_flags: ModFlags::NONE,
    }
}

#[test]
fn a_wheel_over_a_plain_pane_scrolls_by_the_viewers_own_line_count() {
    let pane = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    let mut viewer = build_test_client_with_mouse_config(PartialMouseConfig {
        scroll_line_count: Some(7),
        ..PartialMouseConfig::default()
    });

    let decision = viewer
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&frame, 0)),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(decision.hovered_pane_id, Some(pane));
    assert_eq!(
        decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id: pane,
            is_scrolling_up: true,
            scroll_line_count: 7,
        })
    );
}

#[test]
fn a_wheel_down_over_a_plain_pane_moves_the_view_toward_live() {
    let pane = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));

    let decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Down, get_content_cell(&frame, 0)),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id: pane,
            is_scrolling_up: false,
            scroll_line_count: 3,
        })
    );
}

#[test]
fn a_horizontal_wheel_over_a_plain_pane_decides_nothing() {
    let pane = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));

    for direction in [ScrollDirection::Left, ScrollDirection::Right] {
        let decision = build_test_client()
            .handle_mouse_wheel(
                build_mouse_wheel(direction, get_content_cell(&frame, 0)),
                &frame,
            )
            .expect("a wheel tick decides");

        assert_eq!(
            decision.hovered_pane_id,
            Some(pane),
            "{direction:?} still hovers"
        );
        assert_eq!(
            decision.mouse_action, None,
            "{direction:?} moves no vertical view"
        );
    }
}

#[test]
fn the_ignore_setting_leaves_a_plain_pane_alone() {
    let pane = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    let mut viewer = build_test_client_with_mouse_config(PartialMouseConfig {
        wheel: Some(WheelScroll::Ignore),
        ..PartialMouseConfig::default()
    });

    let decision = viewer
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&frame, 0)),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(decision.mouse_action, None);
}

#[test]
fn a_program_asking_for_the_mouse_gets_the_tick_forwarded() {
    let pane = PaneId::new();
    let mut content = build_plain_mouse_pane(pane);
    content.mouse_tracking = MouseTracking::Normal;
    let frame = build_one_pane_mouse_frame(content);
    let tick = build_mouse_wheel(ScrollDirection::Up, get_content_cell(&frame, 0));

    let decision = build_test_client()
        .handle_mouse_wheel(tick, &frame)
        .expect("a wheel tick decides");

    assert_eq!(
        decision.mouse_action,
        Some(MouseAction::Forward {
            pane_id: pane,
            mouse_input: tick,
        })
    );
}

#[test]
fn x10_tracking_predates_the_wheel_so_the_tick_is_koshis() {
    // `?9` reports presses only. A wheel tick there is not the program's, so it
    // falls through to koshi's own scrollback.
    let pane = PaneId::new();
    let mut content = build_plain_mouse_pane(pane);
    content.mouse_tracking = MouseTracking::X10;
    let frame = build_one_pane_mouse_frame(content);

    let decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&frame, 0)),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id: pane,
            is_scrolling_up: true,
            scroll_line_count: 3,
        })
    );
}

#[test]
fn a_highlight_holds_the_view_even_over_a_mouse_reporting_program() {
    let pane = PaneId::new();
    let mut content = build_plain_mouse_pane(pane);
    content.mouse_tracking = MouseTracking::Normal;
    content.has_selection = true;
    let frame = build_one_pane_mouse_frame(content);

    let decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&frame, 0)),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id: pane,
            is_scrolling_up: true,
            scroll_line_count: 3,
        }),
        "the highlight wins over the program's mouse mode"
    );
}

#[test]
fn the_alternate_screen_with_alt_scroll_becomes_arrow_keys() {
    let pane = PaneId::new();
    let mut content = build_plain_mouse_pane(pane);
    content.is_on_alternate_screen = true;
    content.is_alternate_scroll_enabled = true;
    let frame = build_one_pane_mouse_frame(content);

    let up = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&frame, 0)),
            &frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(
        up.mouse_action,
        Some(MouseAction::AltScrollArrows {
            pane_id: pane,
            is_scrolling_up: true,
            arrow_count: 3,
        })
    );

    let down = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Down, get_content_cell(&frame, 0)),
            &frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(
        down.mouse_action,
        Some(MouseAction::AltScrollArrows {
            pane_id: pane,
            is_scrolling_up: false,
            arrow_count: 3,
        })
    );
}

#[test]
fn alt_scroll_off_the_alternate_screen_is_not_arrow_keys() {
    // `?1007` only translates the wheel while the alternate screen is up.
    let pane = PaneId::new();
    let mut content = build_plain_mouse_pane(pane);
    content.is_alternate_scroll_enabled = true;
    let frame = build_one_pane_mouse_frame(content);

    let decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&frame, 0)),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id: pane,
            is_scrolling_up: true,
            scroll_line_count: 3,
        })
    );
}

#[test]
fn a_horizontal_wheel_under_alt_scroll_sends_no_arrows() {
    let pane = PaneId::new();
    let mut content = build_plain_mouse_pane(pane);
    content.is_on_alternate_screen = true;
    content.is_alternate_scroll_enabled = true;
    let frame = build_one_pane_mouse_frame(content);

    let decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Left, get_content_cell(&frame, 0)),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(decision.mouse_action, None);
}

#[test]
fn the_tick_targets_the_pane_under_the_pointer_not_the_focused_one() {
    let focused_pane_id = PaneId::new();
    let other_pane_id = PaneId::new();
    let frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(focused_pane_id),
            build_plain_mouse_pane(other_pane_id),
        ],
        Some(focused_pane_id),
        PaneKind::Terminal,
    );

    let decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&frame, 1)),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(decision.hovered_pane_id, Some(other_pane_id));
    assert_eq!(
        decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id: other_pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        })
    );
}

#[test]
fn a_tick_over_chrome_falls_through_to_the_focused_pane_and_hovers_nothing() {
    let focused_pane_id = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(focused_pane_id));

    // The hint bar on the bottom row is build_viewer_chrome: no pane sits under the pointer.
    let decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(
                ScrollDirection::Up,
                Point {
                    column: 40,
                    row: TEST_VIEWPORT_SIZE.row_count - 1,
                },
            ),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        decision.hovered_pane_id, None,
        "build_viewer_chrome hovers no pane"
    );
    assert_eq!(
        decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id: focused_pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        })
    );
}

#[test]
fn a_tick_over_chrome_with_a_plugin_pane_focused_decides_nothing() {
    let focused_pane_id = PaneId::new();
    let frame = build_mouse_frame(
        &[build_plain_mouse_pane(focused_pane_id)],
        Some(focused_pane_id),
        PaneKind::Plugin {
            plugin_id: PluginId::new(),
        },
    );

    let decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(
                ScrollDirection::Up,
                Point {
                    column: 40,
                    row: TEST_VIEWPORT_SIZE.row_count - 1,
                },
            ),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(decision.mouse_action, None, "a plugin pane runs no program");
}

#[test]
fn a_tick_over_the_tabline_steps_the_viewers_own_strip() {
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let tab = frame.client_snapshot.active_tab_id;

    let mut viewer = build_test_client();
    let down = viewer
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Down, Point { column: 40, row: 0 }),
            &frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(down.hovered_pane_id, None);
    assert_eq!(
        down.mouse_action, None,
        "the strip is the viewer's own to move"
    );
    assert_eq!(viewer.build_viewer_chrome(tab).tabline_offset, Some(1));

    let up = viewer
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, Point { column: 40, row: 0 }),
            &frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(up.mouse_action, None);
    assert_eq!(
        viewer.build_viewer_chrome(tab).tabline_offset,
        Some(0),
        "the first visible index saturates at zero"
    );
}

#[test]
fn a_tab_switch_cancels_the_strip_peek() {
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let tab = frame.client_snapshot.active_tab_id;
    let mut viewer = build_test_client();

    viewer
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Down, Point { column: 40, row: 0 }),
            &frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(viewer.build_viewer_chrome(tab).tabline_offset, Some(1));

    assert_eq!(
        viewer.build_viewer_chrome(TabId::new()).tabline_offset,
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
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let first_tab = frame.client_snapshot.active_tab_id;
    let other_tab = TabId::new();
    let mut viewer = build_test_client();

    viewer
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Down, Point { column: 40, row: 0 }),
            &frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(
        viewer.build_viewer_chrome(first_tab).tabline_offset,
        Some(1)
    );

    viewer.note_active_tab(other_tab);
    assert_eq!(viewer.build_viewer_chrome(other_tab).tabline_offset, None);

    viewer.note_active_tab(first_tab);
    assert_eq!(
        viewer.build_viewer_chrome(first_tab).tabline_offset,
        None,
        "the peek was thrown away on the switch, not just ignored"
    );
}

#[test]
fn a_mouse_event_on_another_tabs_frame_throws_the_peek_away() {
    // The viewer also learns the tab from the frame a mouse event is answered
    // against, so a peek made on one tab does not come back after an event on
    // another.
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let first_tab = frame.client_snapshot.active_tab_id;
    let mut other_tab_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    other_tab_frame.client_snapshot.active_tab_id = TabId::new();
    let mut viewer = build_test_client();

    viewer
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Down, Point { column: 40, row: 0 }),
            &frame,
        )
        .expect("a wheel tick decides");
    assert_eq!(
        viewer.build_viewer_chrome(first_tab).tabline_offset,
        Some(1)
    );

    viewer.handle_mouse(
        MouseInput {
            mouse_kind: MouseKind::Motion,
            position: get_content_cell(&other_tab_frame, 0),
            modifier_flags: ModFlags::NONE,
        },
        &other_tab_frame,
        Instant::now(),
    );

    assert_eq!(viewer.build_viewer_chrome(first_tab).tabline_offset, None);
}

#[test]
fn every_kind_but_the_wheel_is_left_to_the_session() {
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let screen_point = get_content_cell(&frame, 0);

    for mouse_kind in [
        MouseKind::Press(MouseButton::Left),
        MouseKind::Press(MouseButton::Middle),
        MouseKind::Press(MouseButton::Right),
        MouseKind::Release(MouseButton::Left),
        MouseKind::Drag(MouseButton::Left),
        MouseKind::Motion,
    ] {
        assert_eq!(
            build_test_client().handle_mouse_wheel(
                MouseInput {
                    mouse_kind,
                    position: screen_point,
                    modifier_flags: ModFlags::NONE,
                },
                &frame,
            ),
            None,
            "{mouse_kind:?} is not the viewer's to answer"
        );
    }
}

#[test]
fn a_tick_over_chrome_with_nothing_focused_decides_nothing() {
    // A tab with no focusable pane leaves a tick over build_viewer_chrome with no target.
    let frame = build_mouse_frame(
        &[build_plain_mouse_pane(PaneId::new())],
        None,
        PaneKind::Terminal,
    );

    let decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(
                ScrollDirection::Up,
                Point {
                    column: 40,
                    row: TEST_VIEWPORT_SIZE.row_count - 1,
                },
            ),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(decision.hovered_pane_id, None);
    assert_eq!(decision.mouse_action, None);
}

#[test]
fn a_tick_over_chrome_ignores_a_focused_pane_the_layout_does_not_place() {
    // The focused pane is not among this tab's solved slots, so nothing is
    // drawn for it and a tick that fell through to it targets nothing.
    let mut frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    frame.client_snapshot.focused_pane_id = Some(PaneId::new());

    let decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(
                ScrollDirection::Up,
                Point {
                    column: 40,
                    row: TEST_VIEWPORT_SIZE.row_count - 1,
                },
            ),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(decision.mouse_action, None);
}

#[test]
fn a_zero_line_scroll_setting_is_carried_through_as_zero() {
    // `scroll_lines 0` is the user asking a notch to move nothing. It is
    // carried as a zero-line movement rather than falling back to a default.
    let pane = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    let mut viewer = build_test_client_with_mouse_config(PartialMouseConfig {
        scroll_line_count: Some(0),
        ..PartialMouseConfig::default()
    });

    let decision = viewer
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&frame, 0)),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        decision.mouse_action,
        Some(MouseAction::Scroll {
            pane_id: pane,
            is_scrolling_up: true,
            scroll_line_count: 0,
        })
    );
}

#[test]
fn a_frame_with_no_room_falls_through_to_the_focused_pane() {
    // Every pane is suppressed, so the frame is the "terminal too small"
    // overlay: no tab strip and no pane content is drawn, and nothing is
    // hit-testable.
    let pane = PaneId::new();
    let mut frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    frame
        .session_snapshot
        .active_tab_snapshot
        .are_all_panes_suppressed = true;

    for screen_point in [
        Point { column: 40, row: 0 },
        Point {
            column: 40,
            row: 10,
        },
        Point {
            column: 40,
            row: TEST_VIEWPORT_SIZE.row_count - 1,
        },
    ] {
        let decision = build_test_client()
            .handle_mouse_wheel(
                build_mouse_wheel(ScrollDirection::Down, screen_point),
                &frame,
            )
            .expect("a wheel tick decides");

        assert_eq!(
            decision.hovered_pane_id, None,
            "{screen_point:?} hovers no pane"
        );
        assert_eq!(
            decision.mouse_action,
            Some(MouseAction::Scroll {
                pane_id: pane,
                is_scrolling_up: false,
                scroll_line_count: 3,
            }),
            "{screen_point:?} falls through to the focused pane, and never to the tab strip"
        );
    }
}

#[test]
fn a_tick_aimed_at_a_pane_the_frame_carries_no_content_for_decides_nothing() {
    // The slot is laid out but no `PaneSnapshot` came with it, so nothing is
    // known about the pane's modes and no decision can be made about it.
    let pane = PaneId::new();
    let mut frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    frame.mouse_panes.clear();

    let decision = build_test_client()
        .handle_mouse_wheel(
            build_mouse_wheel(ScrollDirection::Up, get_content_cell(&frame, 0)),
            &frame,
        )
        .expect("a wheel tick decides");

    assert_eq!(
        decision.hovered_pane_id,
        Some(pane),
        "the layout still places it"
    );
    assert_eq!(decision.mouse_action, None);
}

// ============================================================================
// Gestures other than the wheel
// ============================================================================

/// A press, drag, or release of the left button at `position`, with
/// `modifier_flags` held.
fn build_mouse_event(
    mouse_kind: MouseKind,
    position: Point,
    modifier_flags: ModFlags,
) -> MouseInput {
    MouseInput {
        mouse_kind,
        position,
        modifier_flags,
    }
}

/// A left press at `position`.
fn build_left_mouse_press(position: Point) -> MouseInput {
    build_mouse_event(
        MouseKind::Press(MouseButton::Left),
        position,
        ModFlags::NONE,
    )
}

/// A left drag to `position`.
fn build_left_mouse_drag(position: Point) -> MouseInput {
    build_mouse_event(MouseKind::Drag(MouseButton::Left), position, ModFlags::NONE)
}

/// A left release at `position`.
fn build_left_mouse_release(position: Point) -> MouseInput {
    build_mouse_event(
        MouseKind::Release(MouseButton::Left),
        position,
        ModFlags::NONE,
    )
}

/// A buttonless move to `position`.
fn build_mouse_motion(position: Point) -> MouseInput {
    build_mouse_event(MouseKind::Motion, position, ModFlags::NONE)
}

/// An instant `second_count` seconds after `base`, so two presses never read as
/// a double click.
fn advance_time_by_seconds(base: Instant, second_count: u64) -> Instant {
    base + Duration::from_secs(second_count)
}

/// The single `SetSelection` in `actions`, or `None` when it holds none.
fn find_set_selection_action(actions: &[MouseAction]) -> Option<SetSelectionArgs> {
    actions.iter().find_map(|action| match action {
        MouseAction::Command(Command::Visual(VisualCommand::SetSelection(command_args))) => {
            Some(*command_args)
        }
        _ => None,
    })
}

/// The single `Copy` in `actions`, or `None` when it holds none.
fn find_copy_action(actions: &[MouseAction]) -> Option<CopyArgs> {
    actions.iter().find_map(|action| match action {
        MouseAction::Command(Command::Visual(VisualCommand::Copy(command_args))) => {
            Some(*command_args)
        }
        _ => None,
    })
}

#[test]
fn compute_resize_cell_delta_grows_toward_each_border_and_ignores_the_other_axis() {
    let from = Point {
        column: 10,
        row: 10,
    };
    // Right border: pointer rightward grows, leftward shrinks.
    assert_eq!(
        compute_resize_cell_delta(
            Direction::Right,
            from,
            Point {
                column: 13,
                row: 10
            }
        ),
        3
    );
    assert_eq!(
        compute_resize_cell_delta(Direction::Right, from, Point { column: 8, row: 10 }),
        -2
    );
    // Left border: pointer leftward grows.
    assert_eq!(
        compute_resize_cell_delta(Direction::Left, from, Point { column: 7, row: 10 }),
        3
    );
    assert_eq!(
        compute_resize_cell_delta(
            Direction::Left,
            from,
            Point {
                column: 12,
                row: 10
            }
        ),
        -2
    );
    // Down border: pointer downward grows.
    assert_eq!(
        compute_resize_cell_delta(
            Direction::Down,
            from,
            Point {
                column: 10,
                row: 14
            }
        ),
        4
    );
    // Up border: pointer upward grows.
    assert_eq!(
        compute_resize_cell_delta(Direction::Up, from, Point { column: 10, row: 6 }),
        4
    );
    // A left/right border ignores vertical motion.
    assert_eq!(
        compute_resize_cell_delta(
            Direction::Right,
            from,
            Point {
                column: 10,
                row: 20
            }
        ),
        0
    );
}

#[test]
fn advance_resize_anchor_walks_the_anchor_the_way_the_answered_step_asked_and_saturates() {
    let from = Point { column: 3, row: 3 };
    // A positive step grows the pane: a right or down border walks away from
    // zero, a left or up border walks toward it.
    assert_eq!(
        advance_resize_anchor(Direction::Right, from, 1, 2),
        Point { column: 5, row: 3 }
    );
    assert_eq!(
        advance_resize_anchor(Direction::Left, from, 1, 2),
        Point { column: 1, row: 3 }
    );
    assert_eq!(
        advance_resize_anchor(Direction::Down, from, 1, 2),
        Point { column: 3, row: 5 }
    );
    assert_eq!(
        advance_resize_anchor(Direction::Up, from, 1, 2),
        Point { column: 3, row: 1 }
    );
    // A negative step shrinks it, so every border walks the other way.
    assert_eq!(
        advance_resize_anchor(Direction::Right, from, -1, 2),
        Point { column: 1, row: 3 }
    );
    assert_eq!(
        advance_resize_anchor(Direction::Left, from, -1, 2),
        Point { column: 5, row: 3 }
    );
    assert_eq!(
        advance_resize_anchor(Direction::Down, from, -1, 2),
        Point { column: 3, row: 1 }
    );
    assert_eq!(
        advance_resize_anchor(Direction::Up, from, -1, 2),
        Point { column: 3, row: 5 }
    );
    // The anchor lands exactly where `compute_resize_cell_delta` reads the move back.
    for side in [
        Direction::Left,
        Direction::Right,
        Direction::Up,
        Direction::Down,
    ] {
        for step in [-1, 1] {
            assert_eq!(
                compute_resize_cell_delta(side, from, advance_resize_anchor(side, from, step, 2),),
                step * 2,
                "{side:?} answered a step of {step}"
            );
        }
    }
    // Saturating: an anchor at an edge cannot wrap past either end.
    assert_eq!(
        advance_resize_anchor(Direction::Left, from, 1, 10),
        Point { column: 0, row: 3 }
    );
    assert_eq!(
        advance_resize_anchor(Direction::Up, from, 1, 10),
        Point { column: 3, row: 0 }
    );
    assert_eq!(
        advance_resize_anchor(
            Direction::Right,
            Point {
                column: u16::MAX - 1,
                row: 3
            },
            1,
            5
        ),
        Point {
            column: u16::MAX,
            row: 3
        }
    );
}

#[test]
fn a_press_names_the_line_the_frame_showed_on_that_row() {
    // The load-bearing claim of absolute anchoring: the frame says which line
    // the pane's top visible row is, so the press names that line plus the row
    // it landed on — whatever the pane's live view has done since.
    let pane = PaneId::new();
    let mut content = build_plain_mouse_pane(pane);
    content.view_top_row_index = 940;
    let frame = build_one_pane_mouse_frame(content);
    let mut viewer = build_test_client();
    let now = Instant::now();

    // The second visible row of the pane's content.
    let inner = frame.session_snapshot.active_tab_snapshot.pane_slots[0]
        .content_rect
        .expect("a visible pane");
    let screen_point = Point {
        column: inner.origin.column + 4,
        row: inner.origin.row + 3,
    };
    viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
    let actions = viewer.handle_mouse(
        build_left_mouse_drag(Point {
            column: inner.origin.column + 9,
            row: inner.origin.row + 3,
        }),
        &frame,
        advance_time_by_seconds(now, 1),
    );

    let selection_args =
        find_set_selection_action(&actions).expect("the drag asked for a highlight");
    assert_eq!(selection_args.pane_id, pane);
    assert_eq!(
        selection_args.selection.anchor,
        GridPosition {
            row_index: 942,
            column_index: 4
        }
    );
    assert_eq!(
        selection_args.selection.cursor,
        GridPosition {
            row_index: 942,
            column_index: 9
        }
    );
}

#[test]
fn output_between_the_paint_and_the_press_does_not_move_what_it_names() {
    // The same press against the same painted frame names the same line even
    // once the pane has pushed more output: the frame's line numbers are
    // absolute, so nothing about them shifts.
    let pane = PaneId::new();
    let mut scrolled = build_plain_mouse_pane(pane);
    scrolled.view_top_row_index = 500;
    let painted = build_one_pane_mouse_frame(scrolled);
    let inner = painted.session_snapshot.active_tab_snapshot.pane_slots[0]
        .content_rect
        .expect("a visible pane");
    let screen_point = Point {
        column: inner.origin.column,
        row: inner.origin.row + 4,
    };
    let now = Instant::now();

    let mut viewer = build_test_client();
    viewer.handle_mouse(build_left_mouse_press(screen_point), &painted, now);
    let actions = viewer.handle_mouse(
        build_left_mouse_drag(Point {
            column: inner.origin.column + 2,
            row: inner.origin.row + 4,
        }),
        &painted,
        advance_time_by_seconds(now, 1),
    );

    assert_eq!(
        find_set_selection_action(&actions)
            .expect("a highlight")
            .selection
            .anchor,
        GridPosition {
            row_index: 503,
            column_index: 0
        },
        "the press names the line the user saw on that row"
    );
}

#[test]
fn a_second_press_inside_the_threshold_selects_whole_words() {
    let pane = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    let screen_point = get_content_cell(&frame, 0);
    let mut viewer = build_test_client();
    let now = Instant::now();

    viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
    let actions = viewer.handle_mouse(
        build_left_mouse_press(screen_point),
        &frame,
        now + Duration::from_millis(120),
    );

    assert_eq!(
        find_set_selection_action(&actions)
            .expect("a word highlight")
            .selection
            .selection_kind,
        SelectionKind::Word,
        "the second press in the run names a word"
    );
}

#[test]
fn a_third_press_selects_whole_lines_and_a_fourth_starts_over() {
    let pane = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    let screen_point = get_content_cell(&frame, 0);
    let mut viewer = build_test_client();
    let now = Instant::now();

    viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
    viewer.handle_mouse(
        build_left_mouse_press(screen_point),
        &frame,
        now + Duration::from_millis(100),
    );
    let third = viewer.handle_mouse(
        build_left_mouse_press(screen_point),
        &frame,
        now + Duration::from_millis(200),
    );
    assert_eq!(
        find_set_selection_action(&third)
            .expect("a line highlight")
            .selection
            .selection_kind,
        SelectionKind::Line
    );

    // A fourth press starts the run over, and a single click names a point, so
    // it highlights nothing until the pointer moves.
    let fourth = viewer.handle_mouse(
        build_left_mouse_press(screen_point),
        &frame,
        now + Duration::from_millis(300),
    );
    assert_eq!(
        find_set_selection_action(&fourth),
        None,
        "the run began again"
    );
    let dragged = viewer.handle_mouse(
        build_left_mouse_drag(Point {
            column: screen_point.column + 3,
            ..screen_point
        }),
        &frame,
        now + Duration::from_millis(320),
    );
    assert_eq!(
        find_set_selection_action(&dragged)
            .expect("a highlight")
            .selection
            .selection_kind,
        SelectionKind::Character
    );
}

#[test]
fn a_press_past_the_threshold_is_another_single_click() {
    let pane = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    let screen_point = get_content_cell(&frame, 0);
    let mut viewer = build_test_client();
    let now = Instant::now();

    viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
    viewer.handle_mouse(
        build_left_mouse_press(screen_point),
        &frame,
        now + Duration::from_millis(400),
    );
    let actions = viewer.handle_mouse(
        build_left_mouse_drag(Point {
            column: screen_point.column + 3,
            ..screen_point
        }),
        &frame,
        now + Duration::from_millis(420),
    );

    assert_eq!(
        find_set_selection_action(&actions)
            .expect("a highlight")
            .selection
            .selection_kind,
        SelectionKind::Character,
        "exactly 400ms is no longer inside the threshold"
    );
}

#[test]
fn alt_held_at_the_press_makes_a_block_whatever_the_run_of_clicks_was() {
    let pane = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    let screen_point = get_content_cell(&frame, 0);
    let mut viewer = build_test_client();
    let now = Instant::now();

    viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
    let actions = viewer.handle_mouse(
        build_mouse_event(
            MouseKind::Press(MouseButton::Left),
            screen_point,
            ModFlags::ALT,
        ),
        &frame,
        now + Duration::from_millis(120),
    );
    // A block names a point, so the press alone highlights nothing.
    assert_eq!(find_set_selection_action(&actions), None);

    let dragged = viewer.handle_mouse(
        build_left_mouse_drag(Point {
            column: screen_point.column + 2,
            ..screen_point
        }),
        &frame,
        now + Duration::from_millis(140),
    );
    assert_eq!(
        find_set_selection_action(&dragged)
            .expect("a highlight")
            .selection
            .selection_kind,
        SelectionKind::Block
    );
}

#[test]
fn a_captured_drag_that_leaves_the_pane_still_reaches_it_and_the_release_ends_it() {
    let pane = PaneId::new();
    let mut content = build_plain_mouse_pane(pane);
    // Button-event tracking reports presses, drags, and releases.
    content.mouse_tracking = MouseTracking::ButtonMotion;
    let frame = build_one_pane_mouse_frame(content);
    let screen_point = get_content_cell(&frame, 0);
    let mut viewer = build_test_client();
    let now = Instant::now();

    let pressed = viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
    assert_eq!(
        pressed,
        vec![MouseAction::Forward {
            pane_id: pane,
            mouse_input: build_left_mouse_press(screen_point),
        }]
    );
    // The session wrote that press to the pane, which is what captures the
    // gesture; the loop reports it back.
    viewer.note_press_forwarded(pane, MouseButton::Left);

    // Row 0 is the tabline — off the pane entirely. The capture still routes it.
    let outside = Point { column: 0, row: 0 };
    let dragged = viewer.handle_mouse(
        build_left_mouse_drag(outside),
        &frame,
        advance_time_by_seconds(now, 1),
    );
    assert_eq!(
        dragged,
        vec![MouseAction::Forward {
            pane_id: pane,
            mouse_input: build_left_mouse_drag(outside),
        }],
        "the held button keeps the gesture on the pane it pressed"
    );

    let released = viewer.handle_mouse(
        build_left_mouse_release(outside),
        &frame,
        advance_time_by_seconds(now, 2),
    );
    assert_eq!(
        released,
        vec![MouseAction::Forward {
            pane_id: pane,
            mouse_input: build_left_mouse_release(outside),
        }]
    );

    // The capture is over: a further drag reaches no program.
    assert_eq!(
        viewer.handle_mouse(
            build_left_mouse_drag(outside),
            &frame,
            advance_time_by_seconds(now, 3)
        ),
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
    let focused_pane_id = PaneId::new();
    let mut content = build_plain_mouse_pane(focused_pane_id);
    // Any-event tracking asks for every move, so the position is the only thing
    // keeping this one out.
    content.mouse_tracking = MouseTracking::AnyMotion;
    let frame = build_mouse_frame(
        &[content, build_plain_mouse_pane(PaneId::new())],
        Some(focused_pane_id),
        PaneKind::Terminal,
    );
    let inside = get_content_cell(&frame, 0);
    let outside = get_content_cell(&frame, 1);
    let now = Instant::now();
    let mut viewer = build_test_client();

    assert_eq!(
        viewer.handle_mouse(build_mouse_motion(inside), &frame, now),
        vec![MouseAction::Forward {
            pane_id: focused_pane_id,
            mouse_input: build_mouse_motion(inside),
        }],
        "a move inside the focused pane's content is the program's"
    );

    assert_eq!(
        viewer.handle_mouse(
            build_mouse_motion(outside),
            &frame,
            advance_time_by_seconds(now, 1)
        ),
        Vec::new(),
        "the same move one band down names no cell in the focused pane"
    );
}

#[test]
fn mouse_routing_uses_the_region_solve_committed_with_the_frame() {
    let pane = PaneId::new();
    let mut watched = build_plain_mouse_pane(pane);
    watched.mouse_tracking = MouseTracking::AnyMotion;
    let mut frame = build_mouse_frame(&[watched], Some(pane), PaneKind::Terminal);
    frame
        .session_snapshot
        .active_tab_snapshot
        .effective_cell_size = Size {
        column_count: 60,
        row_count: 22,
    };
    frame.session_snapshot.active_tab_snapshot.pane_slots[0] = PaneSlot {
        pane_id: pane,
        outer_rect: Rect::from_origin_and_size(
            Point { column: 0, row: 0 },
            Size {
                column_count: 60,
                row_count: 22,
            },
        ),
        content_rect: Some(Rect::from_origin_and_size(
            Point { column: 1, row: 1 },
            Size {
                column_count: 58,
                row_count: 20,
            },
        )),
        pane_kind: PaneKind::Terminal,
        is_visible: true,
        is_suppressed: false,
        is_dead: false,
    };
    frame.committed_regions = CommittedRegions::from_solved_regions(
        TEST_VIEWPORT_SIZE,
        solve_region_rects(
            TEST_VIEWPORT_SIZE,
            &[
                RegionGeometry {
                    edge: Edge::Top,
                    extent_cell_count: 1,
                },
                RegionGeometry {
                    edge: Edge::Bottom,
                    extent_cell_count: 1,
                },
                RegionGeometry {
                    edge: Edge::Left,
                    extent_cell_count: 20,
                },
            ],
        ),
        9,
    );
    let mut viewer = build_test_client();

    assert_eq!(
        viewer.handle_mouse(
            build_mouse_motion(Point { column: 12, row: 2 }),
            &frame,
            Instant::now()
        ),
        Vec::new(),
        "the committed left region is not pane content"
    );
    assert_eq!(
        viewer.handle_mouse(
            build_mouse_motion(Point { column: 21, row: 2 }),
            &frame,
            Instant::now()
        ),
        vec![MouseAction::Forward {
            pane_id: pane,
            mouse_input: build_mouse_motion(Point { column: 21, row: 2 }),
        }]
    );
}

#[test]
fn a_gesture_is_dropped_when_its_pane_leaves_the_frame() {
    let pane = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    let screen_point = get_content_cell(&frame, 0);
    let mut viewer = build_test_client();
    let now = Instant::now();

    viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);

    // The pane closes: the next frame does not draw it.
    let replacement_pane_id = PaneId::new();
    let replacement_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(replacement_pane_id));
    let actions = viewer.handle_mouse(
        build_left_mouse_drag(get_content_cell(&replacement_frame, 0)),
        &replacement_frame,
        advance_time_by_seconds(now, 1),
    );

    assert_eq!(
        actions,
        Vec::new(),
        "the drag's pane is not on the frame, so the gesture ended with it"
    );
}

#[test]
fn a_pane_swapping_to_the_alternate_screen_ends_the_selection_drag() {
    // The anchor names a line of the primary screen's text. The alternate
    // screen's rows are different text, so extending onto them would highlight
    // whatever `vim` just drew there.
    let pane = PaneId::new();
    let primary_screen_frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    let screen_point = get_content_cell(&primary_screen_frame, 0);
    let to = Point {
        column: screen_point.column + 4,
        ..screen_point
    };
    let mut viewer = build_test_client();
    let now = Instant::now();

    viewer.handle_mouse(
        build_left_mouse_press(screen_point),
        &primary_screen_frame,
        now,
    );
    assert_eq!(
        find_set_selection_action(&viewer.handle_mouse(
            build_left_mouse_drag(to),
            &primary_screen_frame,
            advance_time_by_seconds(now, 1),
        )),
        Some(SetSelectionArgs {
            pane_id: pane,
            selection: Selection {
                selection_kind: SelectionKind::Character,
                anchor: GridPosition {
                    row_index: 1,
                    column_index: 1
                },
                cursor: GridPosition {
                    row_index: 1,
                    column_index: 5
                },
            },
        }),
        "the drag extends while the pane stays on the screen the press landed on"
    );

    // The program in the pane entered the alternate screen.
    let mut alt_pane = build_plain_mouse_pane(pane);
    alt_pane.is_on_alternate_screen = true;
    let alt = build_one_pane_mouse_frame(alt_pane);

    assert_eq!(
        viewer.handle_mouse(
            build_left_mouse_drag(to),
            &alt,
            advance_time_by_seconds(now, 2)
        ),
        Vec::new(),
        "the drag ended with the screen it was made on"
    );
    assert_eq!(viewer.selection_drag, None);
}

#[test]
fn a_drag_held_past_the_top_edge_scrolls_the_view_back_into_history() {
    let pane = PaneId::new();
    let mut content = build_plain_mouse_pane(pane);
    content.view_top_row_index = 100;
    let frame = build_one_pane_mouse_frame(content);
    let screen_point = get_content_cell(&frame, 0);
    let inner = frame.session_snapshot.active_tab_snapshot.pane_slots[0]
        .content_rect
        .expect("a visible pane");
    // Above the pane's first content row, where the earlier lines are.
    let above = Point {
        column: screen_point.column,
        row: 0,
    };
    let mut viewer = build_test_client();
    let now = Instant::now();

    viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
    viewer.handle_mouse(
        build_left_mouse_drag(above),
        &frame,
        advance_time_by_seconds(now, 1),
    );
    let due = advance_time_by_seconds(now, 1) + Duration::from_millis(15);

    assert_eq!(
        viewer.expire_mouse_scroll(due, &frame),
        vec![MouseAction::Scroll {
            pane_id: pane,
            is_scrolling_up: true,
            scroll_line_count: 1,
        }],
        "past the top edge the view moves up, not down"
    );

    // The session says the view moved back one line; the highlight follows it
    // to the pane's first row.
    let actions = viewer.note_scroll_applied(pane, Some(99), &frame);
    let command_args =
        find_set_selection_action(&actions).expect("the scroll re-extends the highlight");
    assert_eq!(
        command_args.selection.cursor,
        GridPosition {
            row_index: 99,
            column_index: screen_point.column - inner.origin.column,
        },
        "the moving end sits on the first row of the view the scroll revealed"
    );
}

#[test]
fn dragging_the_bare_tab_strip_peeks_one_tab_every_six_cells() {
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let tab = frame.client_snapshot.active_tab_id;
    // The bare tab strip, past the one tab's ribbon.
    let strip = Point { column: 40, row: 0 };
    let mut viewer = build_test_client();
    let now = Instant::now();

    viewer.handle_mouse(build_left_mouse_press(strip), &frame, now);

    // Five cells left of the anchor is short of one whole step.
    viewer.handle_mouse(
        build_left_mouse_drag(Point { column: 35, row: 0 }),
        &frame,
        advance_time_by_seconds(now, 1),
    );
    assert_eq!(viewer.build_viewer_chrome(tab).tabline_offset, Some(0));

    // Twelve cells left of the anchor is two steps toward the last tab.
    viewer.handle_mouse(
        build_left_mouse_drag(Point { column: 28, row: 0 }),
        &frame,
        advance_time_by_seconds(now, 2),
    );
    assert_eq!(viewer.build_viewer_chrome(tab).tabline_offset, Some(2));

    // The whole distance is measured from the anchor, so dragging back past it
    // steps the other way and stops at the first tab.
    viewer.handle_mouse(
        build_left_mouse_drag(Point { column: 52, row: 0 }),
        &frame,
        advance_time_by_seconds(now, 3),
    );
    assert_eq!(
        viewer.build_viewer_chrome(tab).tabline_offset,
        Some(0),
        "the first visible index saturates at zero"
    );
}

#[test]
fn a_border_press_starts_no_resize_when_the_viewer_turned_it_off() {
    let left = PaneId::new();
    let right = PaneId::new();
    let frame = build_mouse_frame(
        &[build_plain_mouse_pane(left), build_plain_mouse_pane(right)],
        Some(left),
        PaneKind::Terminal,
    );
    // The shared divider between the two bands: the second pane's top edge.
    let divider = Point {
        column: 10,
        row: frame.session_snapshot.active_tab_snapshot.pane_slots[1]
            .outer_rect
            .origin
            .row
            + 1,
    };
    let now = Instant::now();

    let mut off = build_test_client_with_mouse_config(PartialMouseConfig {
        can_resize_pane_border: Some(false),
        ..PartialMouseConfig::default()
    });
    off.handle_mouse(build_left_mouse_press(divider), &frame, now);
    assert_eq!(
        off.handle_mouse(
            build_left_mouse_drag(Point {
                row: divider.row + 3,
                ..divider
            }),
            &frame,
            advance_time_by_seconds(now, 1)
        ),
        Vec::new(),
        "border resize is off, so the drag moves nothing"
    );

    // The same gesture with the setting on does move the border.
    let mut on = build_test_client();
    on.handle_mouse(build_left_mouse_press(divider), &frame, now);
    let actions = on.handle_mouse(
        build_left_mouse_drag(Point {
            row: divider.row + 3,
            ..divider
        }),
        &frame,
        advance_time_by_seconds(now, 1),
    );
    assert_eq!(
        actions,
        vec![MouseAction::Resize {
            pane_id: right,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 3,
        }],
        "three cells of travel away from the grabbed top border"
    );
}

#[test]
fn grabbing_a_border_with_no_pane_beside_it_starts_no_resize() {
    let pane = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    // The pane's own left edge: the tab's outer frame, with nothing beyond it.
    let outer = Point {
        column: frame.session_snapshot.active_tab_snapshot.pane_slots[0]
            .outer_rect
            .origin
            .column,
        row: 5,
    };
    let mut viewer = build_test_client();
    let now = Instant::now();

    viewer.handle_mouse(build_left_mouse_press(outer), &frame, now);

    assert_eq!(
        viewer.handle_mouse(
            build_left_mouse_drag(Point {
                column: outer.column + 3,
                ..outer
            }),
            &frame,
            advance_time_by_seconds(now, 1)
        ),
        Vec::new(),
        "the tab's outer frame has no neighbour to resize against"
    );
}

#[test]
fn the_drag_anchor_only_walks_over_the_cells_the_session_accepted() {
    // The pointer travelled three cells, the session took two — the pane it
    // would have shrunk hit its minimum size on the third. The anchor advances
    // by those two, so the very next move asks for the one cell still owed.
    let left = PaneId::new();
    let right = PaneId::new();
    let frame = build_mouse_frame(
        &[build_plain_mouse_pane(left), build_plain_mouse_pane(right)],
        Some(left),
        PaneKind::Terminal,
    );
    let divider = Point {
        column: 10,
        row: frame.session_snapshot.active_tab_snapshot.pane_slots[1]
            .outer_rect
            .origin
            .row
            + 1,
    };
    let three_down = Point {
        row: divider.row + 3,
        ..divider
    };
    let mut viewer = build_test_client();
    let now = Instant::now();

    viewer.handle_mouse(build_left_mouse_press(divider), &frame, now);
    assert_eq!(
        viewer.handle_mouse(
            build_left_mouse_drag(three_down),
            &frame,
            advance_time_by_seconds(now, 1)
        ),
        vec![MouseAction::Resize {
            pane_id: right,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 3,
        }]
    );

    // An answer for another pane's border leaves this drag where it is.
    viewer.note_resize_applied(PaneId::new(), Direction::Up, -1, 1);
    // So does an answer for another side of the very border being dragged.
    viewer.note_resize_applied(right, Direction::Down, -1, 1);
    assert_eq!(
        viewer.handle_mouse(
            build_left_mouse_drag(three_down),
            &frame,
            advance_time_by_seconds(now, 2)
        ),
        vec![MouseAction::Resize {
            pane_id: right,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 3,
        }],
        "neither answer named this border, so the whole distance stands"
    );

    viewer.note_resize_applied(right, Direction::Up, -1, 2);

    assert_eq!(
        viewer.handle_mouse(
            build_left_mouse_drag(three_down),
            &frame,
            advance_time_by_seconds(now, 3)
        ),
        vec![MouseAction::Resize {
            pane_id: right,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 1,
        }],
        "two cells were taken, so one is still owed"
    );
}

#[test]
fn releasing_a_highlight_copies_it_with_the_viewers_own_trim_setting() {
    let pane = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    let screen_point = get_content_cell(&frame, 0);

    for trim in [true, false] {
        let mut viewer = build_test_client();
        viewer.client_config.copy.should_trim_trailing_whitespace = trim;
        let now = Instant::now();

        viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
        viewer.handle_mouse(
            build_left_mouse_drag(Point {
                column: screen_point.column + 4,
                ..screen_point
            }),
            &frame,
            advance_time_by_seconds(now, 1),
        );
        let actions = viewer.handle_mouse(
            build_left_mouse_release(Point {
                column: screen_point.column + 4,
                ..screen_point
            }),
            &frame,
            advance_time_by_seconds(now, 2),
        );

        assert_eq!(
            find_copy_action(&actions).expect("the release is the copy"),
            CopyArgs {
                pane_id: pane,
                clipboard_target: CopyTarget::Osc52,
                should_trim_trailing_whitespace: trim,
            },
            "trim {trim}"
        );
    }
}

#[test]
fn copy_on_select_off_releases_without_copying() {
    let pane = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    let screen_point = get_content_cell(&frame, 0);
    let mut viewer = build_test_client();
    viewer.client_config.copy.should_copy_on_select = false;
    let now = Instant::now();

    viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
    viewer.handle_mouse(
        build_left_mouse_drag(Point {
            column: screen_point.column + 4,
            ..screen_point
        }),
        &frame,
        advance_time_by_seconds(now, 1),
    );
    let actions = viewer.handle_mouse(
        build_left_mouse_release(Point {
            column: screen_point.column + 4,
            ..screen_point
        }),
        &frame,
        advance_time_by_seconds(now, 2),
    );

    assert_eq!(
        find_copy_action(&actions),
        None,
        "the highlight stands, uncopied"
    );
}

#[test]
fn a_drag_held_past_the_bottom_edge_scrolls_on_the_clock() {
    let pane = PaneId::new();
    let mut content = build_plain_mouse_pane(pane);
    content.view_top_row_index = 100;
    let frame = build_one_pane_mouse_frame(content);
    let screen_point = get_content_cell(&frame, 0);
    let inner = frame.session_snapshot.active_tab_snapshot.pane_slots[0]
        .content_rect
        .expect("a visible pane");
    let below = Point {
        column: screen_point.column,
        row: inner.origin.row + inner.cell_size.row_count + 5,
    };
    let mut viewer = build_test_client();
    let now = Instant::now();

    viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
    viewer.handle_mouse(
        build_left_mouse_drag(below),
        &frame,
        advance_time_by_seconds(now, 1),
    );
    let due = advance_time_by_seconds(now, 1) + Duration::from_millis(15);
    assert_eq!(
        viewer.next_mouse_wakeup(advance_time_by_seconds(now, 1)),
        Some(Duration::from_millis(15)),
        "the pointer past the edge asks the loop to wake"
    );

    // The firing asks for one line of scroll and nothing else yet.
    let fired = viewer.expire_mouse_scroll(due, &frame);
    assert_eq!(
        fired,
        vec![MouseAction::Scroll {
            pane_id: pane,
            is_scrolling_up: false,
            scroll_line_count: 1,
        }]
    );

    // The session says the view moved on by one line; the highlight follows it
    // to the pane's last row.
    let actions = viewer.note_scroll_applied(pane, Some(101), &frame);
    let command_args =
        find_set_selection_action(&actions).expect("the scroll re-extends the highlight");
    assert_eq!(
        command_args.selection.cursor,
        GridPosition {
            row_index: 101 + u64::from(inner.cell_size.row_count - 1),
            column_index: screen_point.column - inner.origin.column,
        },
        "the moving end sits on the last row of the view the scroll revealed"
    );
}

#[test]
fn a_scroll_that_moved_nothing_disarms_the_timer() {
    let pane = PaneId::new();
    let mut content = build_plain_mouse_pane(pane);
    content.view_top_row_index = 100;
    let frame = build_one_pane_mouse_frame(content);
    let screen_point = get_content_cell(&frame, 0);
    let inner = frame.session_snapshot.active_tab_snapshot.pane_slots[0]
        .content_rect
        .expect("a visible pane");
    let below = Point {
        column: screen_point.column,
        row: inner.origin.row + inner.cell_size.row_count + 5,
    };
    let mut viewer = build_test_client();
    let now = Instant::now();

    viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
    viewer.handle_mouse(
        build_left_mouse_drag(below),
        &frame,
        advance_time_by_seconds(now, 1),
    );
    let due = advance_time_by_seconds(now, 1) + Duration::from_millis(15);
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
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    let mut viewer = build_test_client();

    viewer.handle_mouse(
        build_mouse_wheel(ScrollDirection::Up, get_content_cell(&frame, 0)),
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
    let focused_pane_id = PaneId::new();
    let other_pane_id = PaneId::new();
    let frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(focused_pane_id),
            build_plain_mouse_pane(other_pane_id),
        ],
        Some(focused_pane_id),
        PaneKind::Terminal,
    );
    let mut viewer = build_test_client();

    let actions = viewer.handle_mouse(
        build_left_mouse_press(get_content_cell(&frame, 1)),
        &frame,
        Instant::now(),
    );

    assert_eq!(
        actions,
        vec![MouseAction::Command(Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(other_pane_id),
            client_id: Some(viewer.get_client_id()),
        }))],
        "the first click focuses and nothing else"
    );
}

#[test]
fn mouse_select_mode_takes_a_drag_back_from_a_mouse_aware_program() {
    let pane = PaneId::new();
    let mut content = build_plain_mouse_pane(pane);
    content.mouse_tracking = MouseTracking::ButtonMotion;
    let frame = build_one_pane_mouse_frame(content);
    let screen_point = get_content_cell(&frame, 0);
    let now = Instant::now();

    // With mouse-select off the press is the program's.
    let mut plain = build_test_client();
    assert_eq!(
        plain.handle_mouse(build_left_mouse_press(screen_point), &frame, now),
        vec![MouseAction::Forward {
            pane_id: pane,
            mouse_input: build_left_mouse_press(screen_point),
        }]
    );

    // With it on, the same press begins a koshi highlight instead.
    let mut grabbing = build_mouse_select_client();
    let actions = grabbing.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
    assert_eq!(
        actions,
        vec![MouseAction::Command(Command::Visual(
            VisualCommand::ClearSelection(ClearSelectionArgs { pane_id: pane })
        ))],
        "the press drops the old highlight and arms a drag"
    );
}

#[test]
fn shift_drag_selects_text_from_a_mouse_aware_program() {
    let pane = PaneId::new();
    let mut content = build_plain_mouse_pane(pane);
    content.mouse_tracking = MouseTracking::ButtonMotion;
    let frame = build_one_pane_mouse_frame(content);
    let screen_point = get_content_cell(&frame, 0);
    let to = Point {
        column: screen_point.column + 4,
        ..screen_point
    };
    let now = Instant::now();
    let mut viewer = build_test_client();

    let shifted_press = build_mouse_event(
        MouseKind::Press(MouseButton::Left),
        screen_point,
        ModFlags::SHIFT,
    );
    assert_eq!(
        viewer.handle_mouse(shifted_press, &frame, now),
        vec![MouseAction::Command(Command::Visual(
            VisualCommand::ClearSelection(ClearSelectionArgs { pane_id: pane })
        ))],
        "Shift makes the press start Koshi selection"
    );

    let shifted_drag = build_mouse_event(MouseKind::Drag(MouseButton::Left), to, ModFlags::SHIFT);
    assert_eq!(
        viewer.handle_mouse(shifted_drag, &frame, advance_time_by_seconds(now, 1)),
        vec![MouseAction::Command(Command::Visual(
            VisualCommand::SetSelection(SetSelectionArgs {
                pane_id: pane,
                selection: Selection {
                    selection_kind: SelectionKind::Character,
                    anchor: GridPosition {
                        row_index: 1,
                        column_index: 1
                    },
                    cursor: GridPosition {
                        row_index: 1,
                        column_index: 5
                    },
                },
            })
        ))],
        "the drag extends the selection instead of reaching the program"
    );
}

#[test]
fn ending_the_gestures_drops_all_four_and_leaves_the_pointer_and_the_strip_alone() {
    let hovered_pane_id = PaneId::new();
    let resized_pane_id = PaneId::new();
    let frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(hovered_pane_id),
            build_plain_mouse_pane(resized_pane_id),
        ],
        Some(hovered_pane_id),
        PaneKind::Terminal,
    );
    let tab = frame.client_snapshot.active_tab_id;
    let screen_point = get_content_cell(&frame, 0);
    // The shared divider between the two bands: the second pane's top edge.
    let divider = Point {
        column: 10,
        row: frame.session_snapshot.active_tab_snapshot.pane_slots[1]
            .outer_rect
            .origin
            .row
            + 1,
    };
    // The bare tab strip, past the one tab's ribbon.
    let strip = Point { column: 40, row: 0 };
    let now = Instant::now();
    let mut viewer = build_test_client();

    // A peeked strip and a hovered pane. Neither is a gesture.
    viewer
        .handle_mouse_wheel(build_mouse_wheel(ScrollDirection::Down, strip), &frame)
        .expect("a wheel tick decides");
    viewer.handle_mouse(build_mouse_motion(screen_point), &frame, now);
    assert_eq!(viewer.build_viewer_chrome(tab).tabline_offset, Some(1));
    assert_eq!(
        viewer.build_viewer_chrome(tab).hovered_pane_id,
        Some(hovered_pane_id)
    );

    // All four gestures under way at once: the presses that began them are
    // spaced so no two read as one double click.
    viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
    viewer.handle_mouse(
        build_left_mouse_press(divider),
        &frame,
        advance_time_by_seconds(now, 1),
    );
    viewer.handle_mouse(
        build_left_mouse_press(strip),
        &frame,
        advance_time_by_seconds(now, 2),
    );
    viewer.note_press_forwarded(resized_pane_id, MouseButton::Left);
    assert_eq!(
        viewer.selection_drag.map(|drag| drag.pane_id),
        Some(hovered_pane_id)
    );
    assert_eq!(
        viewer.resize_drag.map(|drag| drag.pane_id),
        Some(resized_pane_id)
    );
    assert_eq!(
        viewer.tabline_drag.map(|drag| drag.anchor_column),
        Some(strip.column)
    );
    assert_eq!(
        viewer.mouse_capture,
        Some(MouseCapture {
            pane_id: resized_pane_id,
            button: MouseButton::Left,
        })
    );

    viewer.end_mouse_gestures();

    assert_eq!(viewer.selection_drag, None);
    assert_eq!(viewer.resize_drag, None);
    assert_eq!(viewer.tabline_drag, None);
    assert_eq!(viewer.mouse_capture, None);
    assert_eq!(
        viewer.build_viewer_chrome(tab).tabline_offset,
        Some(1),
        "the strip peek stands"
    );
    assert_eq!(
        viewer.build_viewer_chrome(tab).hovered_pane_id,
        Some(hovered_pane_id),
        "the hovered pane stands"
    );
    assert_eq!(
        viewer.handle_mouse(
            build_left_mouse_drag(Point {
                column: screen_point.column + 4,
                ..screen_point
            }),
            &frame,
            advance_time_by_seconds(now, 3),
        ),
        Vec::new(),
        "no gesture is under way, so the drag decides nothing"
    );
}

#[test]
fn a_neighbor_across_the_gap_counts_as_adjacent() {
    let left = PaneId::new();
    let right = PaneId::new();
    let mut frame = build_mouse_frame(
        &[build_plain_mouse_pane(left), build_plain_mouse_pane(right)],
        Some(left),
        PaneKind::Terminal,
    );
    frame.session_snapshot.active_tab_snapshot.gap_cell_count = 2;
    frame.session_snapshot.active_tab_snapshot.pane_slots[0].outer_rect =
        Rect::from_origin_and_size(
            Point { column: 0, row: 0 },
            Size {
                column_count: 40,
                row_count: 20,
            },
        );
    frame.session_snapshot.active_tab_snapshot.pane_slots[1].outer_rect =
        Rect::from_origin_and_size(
            Point { column: 42, row: 0 },
            Size {
                column_count: 38,
                row_count: 20,
            },
        );

    assert!(
        border_has_neighbor(&frame, left, Direction::Right),
        "the right pane starts 2 cells past column 39"
    );
    assert!(
        border_has_neighbor(&frame, right, Direction::Left),
        "the left pane ends 2 cells before column 42"
    );

    frame.session_snapshot.active_tab_snapshot.pane_slots[1]
        .outer_rect
        .origin
        .column = 43;

    assert!(
        !border_has_neighbor(&frame, left, Direction::Right),
        "a 3-cell distance is not the tab's gap"
    );
    assert!(!border_has_neighbor(&frame, right, Direction::Left));

    frame.session_snapshot.active_tab_snapshot.pane_slots[1]
        .outer_rect
        .origin
        .column = 41;

    assert!(
        !border_has_neighbor(&frame, left, Direction::Right),
        "a 1-cell distance is not the tab's gap"
    );
    assert!(!border_has_neighbor(&frame, right, Direction::Left));
}

#[test]
fn a_neighbor_below_the_gap_counts_as_adjacent() {
    let top = PaneId::new();
    let bottom = PaneId::new();
    let mut frame = build_mouse_frame(
        &[build_plain_mouse_pane(top), build_plain_mouse_pane(bottom)],
        Some(top),
        PaneKind::Terminal,
    );
    frame.session_snapshot.active_tab_snapshot.gap_cell_count = 2;
    frame.session_snapshot.active_tab_snapshot.pane_slots[0].outer_rect =
        Rect::from_origin_and_size(
            Point { column: 0, row: 0 },
            Size {
                column_count: 40,
                row_count: 20,
            },
        );
    frame.session_snapshot.active_tab_snapshot.pane_slots[1].outer_rect =
        Rect::from_origin_and_size(
            Point { column: 0, row: 22 },
            Size {
                column_count: 40,
                row_count: 18,
            },
        );

    assert!(
        border_has_neighbor(&frame, top, Direction::Down),
        "the bottom pane starts 2 cells past row 19"
    );
    assert!(
        border_has_neighbor(&frame, bottom, Direction::Up),
        "the top pane ends 2 cells before row 22"
    );

    frame.session_snapshot.active_tab_snapshot.pane_slots[1]
        .outer_rect
        .origin
        .row = 23;

    assert!(
        !border_has_neighbor(&frame, top, Direction::Down),
        "a 3-cell distance is not the tab's gap"
    );
    assert!(!border_has_neighbor(&frame, bottom, Direction::Up));

    frame.session_snapshot.active_tab_snapshot.pane_slots[1]
        .outer_rect
        .origin
        .row = 21;

    assert!(
        !border_has_neighbor(&frame, top, Direction::Down),
        "a 1-cell distance is not the tab's gap"
    );
    assert!(!border_has_neighbor(&frame, bottom, Direction::Up));
}

#[test]
fn ending_the_selection_leaves_the_other_three_gestures_alone() {
    // A key that reaches the pane's program ends the highlight gesture only.
    // The border drag, the strip peek-drag and the captured pane are all still
    // under way.
    let selection_pane_id = PaneId::new();
    let captured_pane_id = PaneId::new();
    let frame = build_mouse_frame(
        &[
            build_plain_mouse_pane(selection_pane_id),
            build_plain_mouse_pane(captured_pane_id),
        ],
        Some(selection_pane_id),
        PaneKind::Terminal,
    );
    let screen_point = get_content_cell(&frame, 0);
    let divider = Point {
        column: 10,
        row: frame.session_snapshot.active_tab_snapshot.pane_slots[1]
            .outer_rect
            .origin
            .row
            + 1,
    };
    let strip = Point { column: 40, row: 0 };
    let now = Instant::now();
    let mut viewer = build_test_client();

    viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
    viewer.handle_mouse(
        build_left_mouse_press(divider),
        &frame,
        advance_time_by_seconds(now, 1),
    );
    viewer.handle_mouse(
        build_left_mouse_press(strip),
        &frame,
        advance_time_by_seconds(now, 2),
    );
    viewer.note_press_forwarded(captured_pane_id, MouseButton::Left);

    viewer.end_mouse_selection();

    assert_eq!(viewer.selection_drag, None);
    assert_eq!(
        viewer.resize_drag.map(|drag| drag.pane_id),
        Some(captured_pane_id)
    );
    assert_eq!(
        viewer.tabline_drag.map(|drag| drag.anchor_column),
        Some(strip.column)
    );
    assert_eq!(
        viewer.mouse_capture,
        Some(MouseCapture {
            pane_id: captured_pane_id,
            button: MouseButton::Left,
        })
    );
}

#[test]
fn a_horizontal_wheel_over_the_tab_strip_steps_it_the_same_way() {
    // Left steps toward the first tab, right toward the last — the same two
    // steps up and down make over the strip.
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let tab = frame.client_snapshot.active_tab_id;
    let strip = Point { column: 40, row: 0 };
    let mut viewer = build_test_client();

    let decision = viewer
        .handle_mouse_wheel(build_mouse_wheel(ScrollDirection::Right, strip), &frame)
        .expect("a wheel tick decides");

    assert_eq!(
        decision,
        WheelDecision {
            hovered_pane_id: None,
            mouse_action: None,
        },
        "the strip is this viewer's own; nothing goes to the session"
    );
    assert_eq!(viewer.build_viewer_chrome(tab).tabline_offset, Some(1));

    viewer
        .handle_mouse_wheel(build_mouse_wheel(ScrollDirection::Left, strip), &frame)
        .expect("a wheel tick decides");

    assert_eq!(viewer.build_viewer_chrome(tab).tabline_offset, Some(0));

    viewer
        .handle_mouse_wheel(build_mouse_wheel(ScrollDirection::Left, strip), &frame)
        .expect("a wheel tick decides");

    assert_eq!(
        viewer.build_viewer_chrome(tab).tabline_offset,
        Some(0),
        "the first tab is as far left as the strip goes"
    );
}

#[test]
fn the_wheel_decision_answers_nothing_for_an_event_that_is_not_a_wheel_tick() {
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(PaneId::new()));
    let screen_point = get_content_cell(&frame, 0);
    let mut viewer = build_test_client();

    assert_eq!(
        viewer.handle_mouse_wheel(build_left_mouse_press(screen_point), &frame),
        None
    );
    assert_eq!(
        viewer.handle_mouse_wheel(build_left_mouse_drag(screen_point), &frame),
        None
    );
    assert_eq!(
        viewer.handle_mouse_wheel(build_left_mouse_release(screen_point), &frame),
        None
    );
    assert_eq!(
        viewer.handle_mouse_wheel(build_mouse_motion(screen_point), &frame),
        None
    );
}

#[test]
fn a_viewer_with_no_selection_drag_asks_the_loop_for_no_wakeup() {
    let viewer = build_test_client();

    assert_eq!(viewer.next_mouse_wakeup(Instant::now()), None);
}

#[test]
fn a_scroll_step_already_due_asks_the_loop_to_wake_at_once() {
    let pane = PaneId::new();
    let frame = build_one_pane_mouse_frame(build_plain_mouse_pane(pane));
    let screen_point = get_content_cell(&frame, 0);
    // One row above the pane's content, which is where the edge scroll arms.
    let above = Point {
        column: screen_point.column,
        row: 0,
    };
    let now = Instant::now();
    let mut viewer = build_test_client();

    viewer.handle_mouse(build_left_mouse_press(screen_point), &frame, now);
    viewer.handle_mouse(build_left_mouse_drag(above), &frame, now);

    assert_eq!(
        viewer.next_mouse_wakeup(now + Duration::from_secs(1)),
        Some(Duration::ZERO),
        "a step already behind the clock asks for no further wait"
    );
}
