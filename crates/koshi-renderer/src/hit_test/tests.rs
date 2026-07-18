//! Tests for mouse hit-testing: chrome rows win over the pane area, a click maps
//! to the pane content, its border side, a stack header, or a tab, the layout is
//! centered and the letterbox margin hits nothing, two clients of different
//! sizes hit-test independently, and degenerate frames are safe.

use super::*;

use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::StackHeader;
use koshi_pane::pane::state::PaneKind;

use crate::snapshot::{
    ClientSnapshot, KeymapHints, PaneSlot, PluginUiSnapshot, RenderSnapshot, SessionSnapshot,
    TabMeta, TabSnapshot,
};
use crate::theme::Theme;

/// A cell rect: origin `(x, y)`, size `cols x rows`.
fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
    Rect {
        origin: Point { x, y },
        size: Size { cols, rows },
    }
}

fn at(x: u16, y: u16) -> Point {
    Point { x, y }
}

/// Build a snapshot from explicit pieces. `panes` are `(id, outer rect,
/// visible)` in effective-layout space; a visible pane's content rect is the
/// outer rect inset by its one-cell border. `tabs` are `(id, name)`, the first
/// marked active. The panes carry no content — hit-testing reads only the slot
/// geometry, never a pane's grid.
fn snap(
    viewport: Size,
    effective: Size,
    panes: &[(PaneId, Rect, bool)],
    headers: &[StackHeader],
    tabs: &[(TabId, &str)],
) -> RenderSnapshot {
    let tab_id = TabId::new();

    let slots = panes
        .iter()
        .map(|(id, outer, visible)| PaneSlot {
            pane_id: *id,
            rect: *outer,
            inner_rect: visible.then(|| outer.inner_with_border()),
            kind: PaneKind::Terminal,
            visible: *visible,
            suppressed: false,
            dead: false,
        })
        .collect();

    let tabs_metadata = tabs
        .iter()
        .enumerate()
        .map(|(index, (id, name))| TabMeta {
            id: *id,
            name: (*name).to_string(),
            index,
            active: index == 0,
        })
        .collect();

    RenderSnapshot {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: "s".to_string(),
            active_tab: TabSnapshot {
                id: tab_id,
                name: "active".to_string(),
                layout_solved: slots,
                effective_size: effective,
                stack_headers: headers.to_vec(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
            },
            tabs_metadata,
        },
        panes: Vec::new(),
        client: ClientSnapshot {
            id: ClientId::new(),
            viewport,
            active_tab: tab_id,
            focused_pane: None,
            hovered_pane: None,
            lock_mode: LockMode::Normal,
            mouse_select: false,
            pending_sequence: None,
            tabline_offset: None,
        },
        plugin_ui: PluginUiSnapshot::default(),
        keymap_hints: KeymapHints::default(),
        theme: Theme::default(),
    }
}

fn header(pane: PaneId, r: Rect) -> StackHeader {
    StackHeader {
        pane,
        rect: r,
        position: 0,
        total: 2,
    }
}

/// A pane filling the whole viewport: content, border sides, and the chrome rows
/// on top of it.
#[test]
fn full_pane_content_border_and_chrome() {
    let pane = PaneId::new();
    let s = snap(
        Size { cols: 40, rows: 10 },
        Size { cols: 40, rows: 10 },
        &[(pane, rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );

    // Inside the border → content.
    assert_eq!(
        hit_test(&s, at(20, 5)),
        HitRegion::PaneContent { pane_id: pane }
    );
    // Left and right border columns.
    assert_eq!(
        hit_test(&s, at(0, 5)),
        HitRegion::PaneBorder {
            pane_id: pane,
            side: Direction::Left
        }
    );
    assert_eq!(
        hit_test(&s, at(39, 5)),
        HitRegion::PaneBorder {
            pane_id: pane,
            side: Direction::Right
        }
    );
    // Top row is the tabline (drawn over the pane), off any tab ribbon here.
    assert_eq!(hit_test(&s, at(20, 0)), HitRegion::Tabline);
    // Bottom row is the hint bar.
    assert_eq!(hit_test(&s, at(20, 9)), HitRegion::Statusline);
}

/// A layout smaller than the viewport centers, exposing the top and bottom
/// border rows, and the surrounding margin hits nothing.
#[test]
fn centered_layout_exposes_top_bottom_borders_and_letterbox() {
    let pane = PaneId::new();
    // content_rect centers 40x10 in 44x14 at origin (2, 2).
    let s = snap(
        Size { cols: 44, rows: 14 },
        Size { cols: 40, rows: 10 },
        &[(pane, rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );

    assert_eq!(
        hit_test(&s, at(22, 7)),
        HitRegion::PaneContent { pane_id: pane }
    );
    // Top border row of the pane, now below the tabline.
    assert_eq!(
        hit_test(&s, at(22, 2)),
        HitRegion::PaneBorder {
            pane_id: pane,
            side: Direction::Up
        }
    );
    // Bottom border row of the pane, above the hint bar.
    assert_eq!(
        hit_test(&s, at(22, 11)),
        HitRegion::PaneBorder {
            pane_id: pane,
            side: Direction::Down
        }
    );
    // Left of the content rect → letterbox margin.
    assert_eq!(hit_test(&s, at(0, 7)), HitRegion::None);
    // A non-chrome row above the content rect → letterbox margin.
    assert_eq!(hit_test(&s, at(22, 1)), HitRegion::None);
}

/// A collapsed stack member's strip hit-tests to its pane.
#[test]
fn stack_header_hits_its_pane() {
    let member = PaneId::new();
    let s = snap(
        Size { cols: 40, rows: 10 },
        Size { cols: 40, rows: 10 },
        &[],
        &[header(member, rect(0, 3, 40, 1))],
        &[],
    );
    assert_eq!(
        hit_test(&s, at(20, 3)),
        HitRegion::StackHeader { pane_id: member }
    );
}

/// Tabs map to their own ids by column; the session block and the gaps between
/// tabs are the bare tabline.
#[test]
fn tabs_hit_by_column() {
    let a = TabId::new();
    let b = TabId::new();
    // session " s " = 3 cols, right block " BASE " = 6, so tabs start at x=4:
    // tab a spans [4, 11), a one-cell gap at 11, tab b spans [12, 19).
    let s = snap(
        Size { cols: 40, rows: 10 },
        Size { cols: 40, rows: 10 },
        &[],
        &[],
        &[(a, "a"), (b, "b")],
    );

    assert_eq!(hit_test(&s, at(5, 0)), HitRegion::Tab { tab_id: a });
    assert_eq!(hit_test(&s, at(15, 0)), HitRegion::Tab { tab_id: b });
    // The one-cell gap between the two ribbons.
    assert_eq!(hit_test(&s, at(11, 0)), HitRegion::Tabline);
    // The session block on the left.
    assert_eq!(hit_test(&s, at(1, 0)), HitRegion::Tabline);
}

/// Scroll arrows hit-test to their scroll targets, and those targets step one
/// tab off the current first-visible index.
#[test]
fn scroll_arrows_hit_test_to_their_targets() {
    use crate::render::tabline_layout;
    use ratatui::layout::Rect as RatatuiRect;

    let ids: Vec<TabId> = (0..8).map(|_| TabId::new()).collect();
    let tabs: Vec<(TabId, &str)> = ids.iter().map(|&id| (id, "tab")).collect();
    let mut s = snap(
        Size { cols: 30, rows: 8 },
        Size { cols: 30, rows: 8 },
        &[],
        &[],
        &tabs,
    );
    // Peek from index 2, so tabs are hidden off both sides.
    s.client.tabline_offset = Some(2);

    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: 30,
        height: 8,
    };
    let layout = tabline_layout(&s, area);
    let (left_x, left_to) = layout.left_arrow.expect("tabs hidden off the left");
    let (right_x, right_to) = layout.right_arrow.expect("tabs hidden off the right");

    assert_eq!(left_to, 1, "left arrow steps one tab toward the start");
    assert_eq!(right_to, 3, "right arrow steps one tab toward the end");
    assert_eq!(
        hit_test(&s, at(left_x, 0)),
        HitRegion::TablineScrollLeft { to: 1 }
    );
    assert_eq!(
        hit_test(&s, at(right_x, 0)),
        HitRegion::TablineScrollRight { to: 3 }
    );
}

/// The too-small overlay and a zero-size viewport hit nothing.
#[test]
fn degenerate_frames_hit_nothing() {
    let pane = PaneId::new();
    let mut suppressed = snap(
        Size { cols: 40, rows: 10 },
        Size { cols: 40, rows: 10 },
        &[(pane, rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );
    suppressed.session.active_tab.all_suppressed = true;
    assert_eq!(hit_test(&suppressed, at(20, 5)), HitRegion::None);

    let zero = snap(
        Size { cols: 0, rows: 0 },
        Size { cols: 0, rows: 0 },
        &[],
        &[],
        &[],
    );
    assert_eq!(hit_test(&zero, at(0, 0)), HitRegion::None);
}

/// Two clients viewing the same layout at different sizes hit-test in their own
/// coordinate spaces.
#[test]
fn two_clients_hit_test_independently() {
    let pane = PaneId::new();
    let small = snap(
        Size { cols: 40, rows: 10 },
        Size { cols: 40, rows: 10 },
        &[(pane, rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );
    let large = snap(
        Size { cols: 44, rows: 14 },
        Size { cols: 40, rows: 10 },
        &[(pane, rect(0, 0, 40, 10), true)],
        &[],
        &[],
    );

    // The small client fills the viewport: (22, 7) is content.
    assert_eq!(
        hit_test(&small, at(22, 7)),
        HitRegion::PaneContent { pane_id: pane }
    );
    // The large client centers the layout: the same cell is content too, but a
    // cell in its margin — where the small client had content — hits nothing.
    assert_eq!(
        hit_test(&large, at(22, 7)),
        HitRegion::PaneContent { pane_id: pane }
    );
    assert_eq!(hit_test(&large, at(1, 7)), HitRegion::None);
}
