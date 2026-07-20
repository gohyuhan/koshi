//! Tests for the tabline solve and paint: which blocks anchor the two edges,
//! which tabs fit the middle window, where the scroll arrows land, and the exact
//! cells and styles `draw_tabline` writes — including tab widths measured by
//! display width for wide (CJK), emoji, and combining-mark titles.

use super::*;

use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;

use crate::snapshot::{
    ClientSnapshot, KeymapHints, PluginUiSnapshot, SessionSnapshot, TabMeta, TabSnapshot,
};

/// Build a tabline-only snapshot. `tabs` are `(name, active)`; there are no
/// panes, since the tabline reads only the session name, the tab metadata, and
/// the client's lock/select/offset state.
fn snap(
    session: &str,
    tabs: &[(&str, bool)],
    tabline_offset: Option<usize>,
    lock_mode: LockMode,
    mouse_select: bool,
) -> RenderSnapshot {
    let tab_id = TabId::new();
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
    let viewport = Size { cols: 200, rows: 1 };
    RenderSnapshot {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: session.to_string(),
            active_tab: TabSnapshot {
                id: tab_id,
                name: "active".to_string(),
                layout_solved: Vec::new(),
                effective_size: viewport,
                stack_headers: Vec::new(),
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
            lock_mode,
            mouse_select,
            pending_sequence: None,
            tabline_offset,
        },
        plugin_ui: PluginUiSnapshot::default(),
        keymap_hints: KeymapHints::default(),
        theme: Theme::default(),
    }
}

/// Cells the `[v0.1.0] ` version badge takes beside the session name: the
/// version string plus `[`, `v`, `]`, and the trailing space. Measured rather
/// than hardcoded, so a version bump that lengthens the string (`0.9.0` →
/// `0.10.0`) shifts every expected column below instead of failing.
const BADGE: u16 = KOSHI_VERSION.len() as u16 + 4;

/// A one-row render area `width` cells wide, anchored at the origin.
fn area(width: u16) -> RatatuiRect {
    RatatuiRect {
        x: 0,
        y: 0,
        width,
        height: 1,
    }
}

/// Paint the tabline into a fresh one-row buffer of `width` cells.
fn draw(snapshot: &RenderSnapshot, width: u16) -> Buffer {
    let a = area(width);
    let mut buf = Buffer::empty(a);
    draw_tabline(snapshot, a, &mut buf);
    buf
}

/// The symbol at cell `x` of the single rendered row.
fn cell(buf: &Buffer, x: u16) -> &str {
    buf[(x, 0)].symbol()
}

// --- geometry: block widths and the fitting window ---------------------------

#[test]
fn one_tab_that_fits_shows_whole_with_no_arrows() {
    // Session block ` s ` (3 cells) plus the version badge; right block
    // " BASE " = 6; the strip starts one cell past the session block, so the
    // tab " #1  a " (7 cells) sits just after the badge.
    let layout = tabline_layout(
        &snap("s", &[("a", true)], None, LockMode::Normal, false),
        area(20 + BADGE),
    );
    assert_eq!(layout.session_width, 3 + BADGE);
    assert_eq!(layout.right_x, 14 + BADGE);
    assert_eq!(layout.first_visible, 0);
    assert_eq!(layout.tabs, vec![(0, 4 + BADGE, 7)]);
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, None);
}

#[test]
fn several_tabs_that_all_fit_pack_left_to_right_with_a_gap() {
    let layout = tabline_layout(
        &snap(
            "s",
            &[("a", false), ("b", false), ("c", true)],
            None,
            LockMode::Normal,
            false,
        ),
        area(40 + BADGE),
    );
    assert_eq!(
        layout.tabs,
        vec![(0, 4 + BADGE, 7), (1, 12 + BADGE, 7), (2, 20 + BADGE, 7)]
    );
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, None);
}

#[test]
fn a_tab_that_exactly_fills_the_gap_is_kept() {
    // One cell wider than the tab needs: the tab's last cell lands exactly on
    // `right_x`, so it just fits and no scrolling begins.
    let layout = tabline_layout(
        &snap("s", &[("a", true)], None, LockMode::Normal, false),
        area(17 + BADGE),
    );
    assert_eq!(layout.tabs, vec![(0, 4 + BADGE, 7)]);
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, None);
}

#[test]
fn one_column_too_narrow_drops_the_tab_and_shows_a_right_arrow() {
    // One cell narrower than that: the tab no longer fits, so the strip
    // scrolls; nothing is visible yet and a right arrow marks the tab hidden
    // off the right edge.
    let layout = tabline_layout(
        &snap("s", &[("a", true)], None, LockMode::Normal, false),
        area(16 + BADGE),
    );
    assert_eq!(layout.first_visible, 0);
    assert!(layout.tabs.is_empty());
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, Some((9 + BADGE, 1)));
}

#[test]
fn following_the_active_tab_scrolls_it_into_view() {
    // The strip holds one tab in the arrow-framed window; with the last tab
    // active and no peek offset, the window starts at it and only a left arrow
    // shows.
    let layout = tabline_layout(
        &snap(
            "s",
            &[("a", false), ("b", false), ("c", true)],
            None,
            LockMode::Normal,
            false,
        ),
        area(24 + BADGE),
    );
    assert_eq!(layout.first_visible, 2);
    assert_eq!(layout.tabs, vec![(2, 5 + BADGE, 7)]);
    assert_eq!(layout.left_arrow, Some((4 + BADGE, 1)));
    assert_eq!(layout.right_arrow, None);
}

#[test]
fn a_peek_offset_windows_from_that_index_with_both_arrows() {
    let layout = tabline_layout(
        &snap(
            "s",
            &[("a", true), ("b", false), ("c", false)],
            Some(1),
            LockMode::Normal,
            false,
        ),
        area(24 + BADGE),
    );
    assert_eq!(layout.first_visible, 1);
    assert_eq!(layout.tabs, vec![(1, 5 + BADGE, 7)]);
    assert_eq!(layout.left_arrow, Some((4 + BADGE, 0)));
    assert_eq!(layout.right_arrow, Some((17 + BADGE, 2)));
}

#[test]
fn a_peek_offset_past_the_last_tab_clamps_to_it() {
    let layout = tabline_layout(
        &snap(
            "s",
            &[("a", true), ("b", false), ("c", false)],
            Some(99),
            LockMode::Normal,
            false,
        ),
        area(24 + BADGE),
    );
    assert_eq!(layout.first_visible, 2);
    assert_eq!(layout.tabs, vec![(2, 5 + BADGE, 7)]);
    assert_eq!(layout.left_arrow, Some((4 + BADGE, 1)));
    assert_eq!(layout.right_arrow, None);
}

#[test]
fn an_empty_tab_list_leaves_only_the_two_blocks() {
    let layout = tabline_layout(
        &snap("s", &[], None, LockMode::Normal, false),
        area(20 + BADGE),
    );
    assert_eq!(layout.session_width, 3 + BADGE);
    assert_eq!(layout.right_x, 14 + BADGE);
    assert_eq!(layout.first_visible, 0);
    assert!(layout.tabs.is_empty());
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, None);
}

#[test]
fn no_room_between_the_blocks_yields_no_tabs() {
    // width 6 is exactly the right block, leaving no strip at all.
    let layout = tabline_layout(
        &snap("s", &[("a", true)], None, LockMode::Normal, false),
        area(6),
    );
    assert_eq!(layout.session_width, 0);
    assert_eq!(layout.right_x, 0);
    assert!(layout.tabs.is_empty());
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, None);
}

// --- the mode tag drives the right block's width -----------------------------

#[test]
fn the_select_mode_tag_widens_the_right_block() {
    // " SELECT " is 8 cells, so the right block starts 8 cells from the right
    // edge — two cells left of where the 6-cell " BASE " block starts.
    let layout = tabline_layout(
        &snap("s", &[("a", true)], None, LockMode::Normal, true),
        area(20 + BADGE),
    );
    assert_eq!(layout.right_x, 12 + BADGE);
}

#[test]
fn a_lock_mode_tag_is_the_same_width_as_base() {
    // " LOCK " and " BASE " are both 6 cells.
    let layout = tabline_layout(
        &snap("s", &[("a", true)], None, LockMode::Locked, false),
        area(20 + BADGE),
    );
    assert_eq!(layout.right_x, 14 + BADGE);
}

// --- display-width titles ----------------------------------------------------

#[test]
fn a_wide_cjk_title_counts_two_cells_per_glyph() {
    // " 字 " is 1 + 2 + 1 = 4 cells, so the tab is " #1 "(4) + 4 = 8 wide.
    let layout = tabline_layout(
        &snap("s", &[("字", true)], None, LockMode::Normal, false),
        area(60 + BADGE),
    );
    assert_eq!(layout.tabs, vec![(0, 4 + BADGE, 8)]);
}

#[test]
fn an_emoji_title_counts_two_cells() {
    let layout = tabline_layout(
        &snap("s", &[("🎉", true)], None, LockMode::Normal, false),
        area(60 + BADGE),
    );
    assert_eq!(layout.tabs, vec![(0, 4 + BADGE, 8)]);
}

#[test]
fn a_combining_mark_title_stays_one_cell() {
    // "e" + combining acute is one display cell: " é " is 3, tab is 4 + 3 = 7.
    let layout = tabline_layout(
        &snap("s", &[("e\u{0301}", true)], None, LockMode::Normal, false),
        area(60 + BADGE),
    );
    assert_eq!(layout.tabs, vec![(0, 4 + BADGE, 7)]);
}

#[test]
fn a_two_digit_tab_number_widens_that_tab() {
    // Tab 9 shows "#10" — a wider `#N` block than the single-digit tabs.
    let tabs: Vec<(&str, bool)> = (0..10).map(|i| ("a", i == 0)).collect();
    let layout = tabline_layout(
        &snap("s", &tabs, None, LockMode::Normal, false),
        area(200 + BADGE),
    );
    assert_eq!(layout.tabs.len(), 10);
    assert_eq!(layout.tabs[8].2, 7);
    assert_eq!(layout.tabs[9].2, 8);
}

// --- painting: exact cells and styles ----------------------------------------

#[test]
fn draw_paints_session_tab_and_mode_with_their_styles() {
    let width = 20 + BADGE;
    let buf = draw(
        &snap("s", &[("a", true)], None, LockMode::Normal, false),
        width,
    );

    // Session block " s " on the left.
    assert_eq!(cell(&buf, 0), " ");
    assert_eq!(cell(&buf, 1), "s");
    assert_eq!(cell(&buf, 2), " ");
    assert_eq!(buf[(1, 0)].fg, Color::Rgb(0xd0, 0xa5, 0xff));
    assert!(buf[(1, 0)].modifier.contains(Modifier::BOLD));

    // Then the version badge `[v0.1.0] `: the same ramp color as the name,
    // without its bold.
    let badge: String = (3..3 + BADGE).map(|x| cell(&buf, x)).collect();
    assert_eq!(badge, format!("[v{KOSHI_VERSION}] "));
    assert_eq!(buf[(4, 0)].fg, Color::Rgb(0xd0, 0xa5, 0xff));
    assert!(!buf[(4, 0)].modifier.contains(Modifier::BOLD));

    // One-cell gap after the badge, then the tab " #1  a ".
    let tab = 4 + BADGE;
    assert_eq!(cell(&buf, tab - 1), " ");
    assert_eq!(cell(&buf, tab), " ");
    assert_eq!(cell(&buf, tab + 1), "#");
    assert_eq!(cell(&buf, tab + 2), "1");
    assert_eq!(cell(&buf, tab + 3), " ");
    assert_eq!(cell(&buf, tab + 4), " ");
    assert_eq!(cell(&buf, tab + 5), "a");
    assert_eq!(cell(&buf, tab + 6), " ");
    // The active tab's `#N` block is its ramp stop as bold text.
    assert_eq!(buf[(tab + 1, 0)].fg, Color::Rgb(0xd0, 0xa5, 0xff));
    assert!(buf[(tab + 1, 0)].modifier.contains(Modifier::BOLD));

    // Right block " BASE " anchored to the right edge, its last 6 cells.
    let base = width - 6;
    assert_eq!(cell(&buf, base), " ");
    assert_eq!(cell(&buf, base + 1), "B");
    assert_eq!(cell(&buf, base + 2), "A");
    assert_eq!(cell(&buf, base + 3), "S");
    assert_eq!(cell(&buf, base + 4), "E");
    assert_eq!(cell(&buf, base + 5), " ");
    assert_eq!(buf[(base + 1, 0)].fg, Color::Rgb(0x7d, 0xbc, 0xff));
    assert!(buf[(base + 1, 0)].modifier.contains(Modifier::BOLD));
}

#[test]
fn a_row_too_narrow_for_the_badge_drops_it_whole() {
    // 16 cells hold the session block and the " BASE " tag but not the badge
    // as well. It is dropped entire rather than clipped — no half-written
    // "[v0.1.0" with its closing bracket cut off. The tab does not fit either,
    // so the strip is just its right arrow.
    let snapshot = snap("s", &[("a", true)], None, LockMode::Normal, false);
    let buf = draw(&snapshot, 16);
    let row: String = (0..16).map(|x| cell(&buf, x)).collect();
    assert_eq!(row, " s       ▶ BASE ");
    assert_eq!(tabline_layout(&snapshot, area(16)).session_width, 3);
}

#[test]
fn draw_fills_the_whole_row_with_the_bar_background() {
    // The session block, badge, and one tab leave the middle empty; every cell
    // of the row still carries the bar background, painted before any text.
    let width = 20 + BADGE;
    let buf = draw(
        &snap("s", &[("a", true)], None, LockMode::Normal, false),
        width,
    );
    for x in 0..width {
        assert_eq!(buf[(x, 0)].bg, Color::Rgb(0x00, 0x00, 0x00), "col {x}");
    }
}

#[test]
fn draw_paints_the_select_tag_when_the_mouse_is_grabbed() {
    let width = 20 + BADGE;
    let buf = draw(
        &snap("s", &[("a", true)], None, LockMode::Normal, true),
        width,
    );
    // " SELECT " fills the row's last 8 cells.
    let tag = width - 8;
    assert_eq!(cell(&buf, tag), " ");
    assert_eq!(cell(&buf, tag + 1), "S");
    assert_eq!(cell(&buf, tag + 2), "E");
    assert_eq!(cell(&buf, tag + 3), "L");
    assert_eq!(cell(&buf, tag + 4), "E");
    assert_eq!(cell(&buf, tag + 5), "C");
    assert_eq!(cell(&buf, tag + 6), "T");
    assert_eq!(cell(&buf, tag + 7), " ");
}

#[test]
fn draw_paints_the_lock_tag_in_locked_mode() {
    let width = 20 + BADGE;
    let buf = draw(
        &snap("s", &[("a", true)], None, LockMode::Locked, false),
        width,
    );
    // " LOCK " fills the row's last 6 cells.
    let tag = width - 6;
    assert_eq!(cell(&buf, tag), " ");
    assert_eq!(cell(&buf, tag + 1), "L");
    assert_eq!(cell(&buf, tag + 2), "O");
    assert_eq!(cell(&buf, tag + 3), "C");
    assert_eq!(cell(&buf, tag + 4), "K");
    assert_eq!(cell(&buf, tag + 5), " ");
}

#[test]
fn draw_paints_the_right_scroll_arrow_when_a_tab_is_hidden() {
    let width = 16 + BADGE;
    let buf = draw(
        &snap("s", &[("a", true)], None, LockMode::Normal, false),
        width,
    );
    // The tab is dropped; a "▶" sits one cell left of the right block.
    let arrow = width - 7;
    assert_eq!(cell(&buf, arrow), "▶");
    assert_eq!(buf[(arrow, 0)].fg, Color::Rgb(0xf0, 0xec, 0xfa));
    assert!(buf[(arrow, 0)].modifier.contains(Modifier::BOLD));
    // Right block " BASE " still anchors the edge.
    assert_eq!(cell(&buf, width - 5), "B");
}

#[test]
fn draw_paints_the_left_scroll_arrow_when_a_tab_is_hidden_left() {
    let buf = draw(
        &snap(
            "s",
            &[("a", true), ("b", false), ("c", false)],
            Some(1),
            LockMode::Normal,
            false,
        ),
        24 + BADGE,
    );
    // Peeking from index 1 hides tab 0 off the left: "◀" at the strip start,
    // one cell past the session block and its badge.
    assert_eq!(cell(&buf, 4 + BADGE), "◀");
    assert_eq!(buf[(4 + BADGE, 0)].fg, Color::Rgb(0xf0, 0xec, 0xfa));
    assert!(buf[(4 + BADGE, 0)].modifier.contains(Modifier::BOLD));
    // And the right arrow marks tab 2 hidden off the right.
    assert_eq!(cell(&buf, 17 + BADGE), "▶");
}
