//! Tests for stock frame composition: the three zones render into a ratatui
//! buffer, tabs show their marker, the mode tag tracks the client lock mode,
//! pane borders draw with focus highlighting, and degenerate sizes are safe.

use super::*;

use std::sync::Arc;

use tile_core::geometry::{Point, Size};
use tile_core::ids::{ClientId, PaneId, SessionId, TabId};
use tile_terminal::grid::state::Grid;
use tile_terminal::style::Style as TermStyle;

use crate::snapshot::{
    ClientSnapshot, CursorSnapshot, GridView, PaneSlot, PaneSnapshot, PluginUiSnapshot,
    ScrollbackMeta, SessionSnapshot, TabMeta, TabSnapshot,
};
use tile_layout::mode::LayoutMode;
use tile_pane::pane::state::PaneKind;

/// A cell rect: origin `(x, y)`, size `cols x rows`.
fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
    Rect {
        origin: Point { x, y },
        size: Size { cols, rows },
    }
}

/// Build a snapshot from explicit pieces. `panes` are `(id, outer rect, visible)`;
/// a visible pane's content rect is the outer rect inset by its one-cell border.
fn build(
    session: &str,
    tabs: &[(&str, bool)],
    panes: &[(PaneId, Rect, bool)],
    focused: Option<PaneId>,
    lock_mode: LockMode,
    viewport: Size,
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

    let pane_snapshots = panes
        .iter()
        .map(|(id, _, _)| PaneSnapshot {
            id: *id,
            title: None,
            cursor: CursorSnapshot {
                row: 0,
                col: 0,
                visible: true,
                blink: false,
            },
            grid_view: None,
            reverse_video: false,
            scrollback: ScrollbackMeta {
                truncated: false,
                retained_lines: 0,
            },
        })
        .collect();

    let tabs_metadata = tabs
        .iter()
        .enumerate()
        .map(|(index, (name, active))| TabMeta {
            id: TabId::new(),
            name: (*name).to_string(),
            index,
            active: *active,
        })
        .collect();

    RenderSnapshot {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: session.to_string(),
            active_tab: TabSnapshot {
                id: tab_id,
                name: "active".to_string(),
                layout_solved: slots,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
            },
            tabs_metadata,
        },
        panes: pane_snapshots,
        client: ClientSnapshot {
            id: ClientId::new(),
            viewport,
            active_tab: tab_id,
            focused_pane: focused,
            lock_mode,
        },
        plugin_ui: PluginUiSnapshot::default(),
    }
}

/// Render a snapshot into a fresh `w x h` buffer.
fn render(snapshot: &RenderSnapshot, w: u16, h: u16) -> Buffer {
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: w,
        height: h,
    };
    let mut buf = Buffer::empty(area);
    render_frame(snapshot, area, &mut buf);
    buf
}

/// The visible text of buffer row `y`.
fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area().width)
        .map(|x| buf[(x, y)].symbol().to_string())
        .collect()
}

#[test]
fn renders_tabline_pane_border_and_reserved_hint_bar() {
    let pane = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    let buf = render(&snap, 40, 8);

    // Tabline (row 0): session + tab on the left, mode tag right-aligned.
    let tabline = row_text(&buf, 0);
    assert!(tabline.starts_with("sess"), "tabline: {tabline:?}");
    assert!(tabline.contains("1:shell"), "tabline: {tabline:?}");
    assert!(
        tabline.trim_end().ends_with("[BASE]"),
        "tabline: {tabline:?}"
    );

    // Pane border box on rows 1..=6, columns 0..=39.
    assert_eq!(buf[(0, 1)].symbol(), "┌");
    assert_eq!(buf[(39, 1)].symbol(), "┐");
    assert_eq!(buf[(0, 6)].symbol(), "└");
    assert_eq!(buf[(39, 6)].symbol(), "┘");
    assert_eq!(buf[(1, 1)].symbol(), "─");
    assert_eq!(buf[(0, 2)].symbol(), "│");

    // Bottom row (row 7): the keybind-hint bar is reserved and blank for now.
    assert!(
        row_text(&buf, 7).trim().is_empty(),
        "hint bar row: {:?}",
        row_text(&buf, 7)
    );
}

#[test]
fn tabline_lists_tabs_with_active_marker() {
    let pane = PaneId::new();
    let snap = build(
        "sess",
        &[("code", true), ("logs", false)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    let buf = render(&snap, 40, 8);

    // "sess │ " is 7 columns, then "1:code" then " " then "2:logs".
    assert!(row_text(&buf, 0).contains("1:code 2:logs"));
    // The active tab is drawn inverted; the inactive one is not.
    assert!(buf[(7, 0)].modifier.contains(Modifier::REVERSED));
    assert!(!buf[(14, 0)].modifier.contains(Modifier::REVERSED));
}

#[test]
fn mode_tag_reflects_lock_mode() {
    let pane = PaneId::new();
    let make = |mode| {
        build(
            "sess",
            &[("shell", true)],
            &[(pane, rect(0, 1, 40, 6), true)],
            Some(pane),
            mode,
            Size { cols: 40, rows: 8 },
        )
    };

    let base = render(&make(LockMode::Normal), 40, 8);
    assert!(row_text(&base, 0).contains("[BASE]"));

    let locked = render(&make(LockMode::Locked), 40, 8);
    assert!(row_text(&locked, 0).contains("[LOCK]"));
    assert!(!row_text(&locked, 0).contains("[BASE]"));
}

#[test]
fn focused_pane_border_is_highlighted() {
    let left = PaneId::new();
    let right = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[
            (left, rect(0, 1, 20, 6), true),
            (right, rect(20, 1, 20, 6), true),
        ],
        Some(left),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    let buf = render(&snap, 40, 8);

    // Focused pane: cyan, bold border corner.
    assert_eq!(buf[(0, 1)].fg, Color::Cyan);
    assert!(buf[(0, 1)].modifier.contains(Modifier::BOLD));
    // Unfocused pane: dim border corner, no bold.
    assert_eq!(buf[(20, 1)].fg, Color::DarkGray);
    assert!(!buf[(20, 1)].modifier.contains(Modifier::BOLD));
}

#[test]
fn hidden_pane_draws_no_border() {
    let pane = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), false)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );
    let buf = render(&snap, 40, 8);

    // No border cell where the box would have been.
    assert_eq!(buf[(0, 1)].symbol(), " ");
    assert_eq!(buf[(1, 1)].symbol(), " ");
}

#[test]
fn scroll_indicator_shown_only_when_scrolled_back() {
    let pane = PaneId::new();
    let mut snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 8 },
    );

    // At the live tail (no grid view / offset 0): no scroll indicator.
    assert!(!row_text(&render(&snap, 40, 8), 0).contains("SCROLL"));

    // Scrolled back three lines with 100 retained: indicator appears.
    snap.panes[0].grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(6, 40, TermStyle::default())),
        view_offset: 3,
    });
    snap.panes[0].scrollback.retained_lines = 100;
    assert!(row_text(&render(&snap, 40, 8), 0).contains("SCROLL 3/100"));
}

#[test]
fn reused_buffer_is_blanked_before_painting() {
    let pane = PaneId::new();
    let snap = build(
        "s",
        &[("t", true)],
        &[(pane, rect(0, 1, 20, 4), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 24, rows: 6 },
    );

    // A buffer reused across frames holds the previous frame's cells; simulate
    // that with a full grid of stale glyphs before rendering.
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: 24,
        height: 6,
    };
    let mut buf = Buffer::empty(area);
    for y in 0..area.height {
        for x in 0..area.width {
            buf[(x, y)].set_symbol("X");
        }
    }

    render_frame(&snap, area, &mut buf);

    // Tabline gap between the left tab list and the right status: blanked.
    assert_eq!(buf[(12, 0)].symbol(), " ");
    // A cell outside every pane box: blanked, not the stale glyph.
    assert_eq!(buf[(22, 2)].symbol(), " ");
    // Reserved hint row (bottom): fully blank.
    assert!(row_text(&buf, 5).chars().all(|c| c == ' '));
}

#[test]
fn small_and_zero_size_areas_are_safe() {
    let pane = PaneId::new();
    let snap = build(
        "sess",
        &[("shell", true)],
        &[(pane, rect(0, 1, 40, 6), true)],
        Some(pane),
        LockMode::Normal,
        Size { cols: 40, rows: 1 },
    );

    // One row tall: only the tabline, no bottom row, no panic.
    let one_row = render(&snap, 40, 1);
    assert!(row_text(&one_row, 0).contains("sess"));

    // Widths narrower than the tabline content (the mode tag is 6 cells): the
    // right-aligned segment saturates and clips instead of underflowing.
    for width in [1, 2, 3, 6] {
        let _ = render(&snap, width, 4);
    }

    // Zero area: nothing drawn, no panic.
    let mut empty = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    });
    render_frame(
        &snap,
        RatatuiRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        &mut empty,
    );
}
